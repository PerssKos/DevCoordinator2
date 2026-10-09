use super::*;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Recovery {
    Restored,
    Deferred,
    NotApplicable,
}

/// Restore only a missing process for the exact committed routed generation.
/// This runs before ordinary route validation, because validation intentionally
/// withdraws a route whose listener is absent. No lease, generation, source,
/// environment or deployment intent is created by this path.
pub(crate) fn restore_route(
    deployments: &Deployments,
    deployment_id: &str,
) -> Result<Recovery, ProtocolError> {
    let route = deployments
        .database
        .call({
            let id = deployment_id.to_owned();
            move |connection| {
                connection
                    .query_row(
                        "SELECT domain,component,port,generation,lease_id FROM domain_routes WHERE deployment_id=?1",
                        [&id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<u16>>(2)?,
                                row.get::<_, Option<u32>>(3)?,
                                row.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            }
        })
        .map_err(database_error)?;
    let Some((domain, component_name, route_port, Some(generation), route_lease)) = route else {
        return Ok(Recovery::NotApplicable);
    };
    let row = deployments
        .store
        .get(deployment_id)?
        .ok_or_else(|| not_found(deployment_id))?;
    if row.current_generation != Some(generation)
        || matches!(row.state.as_str(), "applying" | "starting" | "stopping")
    {
        return Ok(Recovery::Deferred);
    }
    let stored = deployments
        .store
        .components(deployment_id)?
        .into_iter()
        .find(|component| component.name == component_name)
        .ok_or_else(|| {
            ProtocolError::new(ErrorCode::DeploymentNotFound, "routed component is missing")
        })?;
    if stored.desired_state != "running" || stored.generation != Some(generation) {
        return Ok(Recovery::NotApplicable);
    }
    let caller = Caller {
        via_edge: false,
        pid: 0,
        uid: row.created_by_uid,
        gid: primary_gid(row.created_by_uid).map_err(systemd_error)?,
        client_kind: devcoordinator2_api::ClientKind::Other,
        model: None,
        effort: None,
        client_session: None,
        work: None,
        identity: None,
    };
    let mut target = deployments.resolve_target_readonly(None, None, Some(deployment_id), &caller)?;
    // Recovery is bound to the saved declaration. If the checkout changed,
    // leave the route for an explicit apply instead of replaying new config.
    let saved_spec: serde_json::Value = serde_json::from_str(&row.spec_json).map_err(|_| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "stored deployment specification is invalid",
        )
    })?;
    let saved: ComponentSpec = saved_spec
        .get("components")
        .and_then(Value::as_array)
        .and_then(|components| {
            components.iter().find(|value| {
                value.get("name").and_then(Value::as_str) == Some(component_name.as_str())
            })
        })
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "saved routed component is invalid",
            )
        })?;
    if DeploymentStore::component_fingerprint(&saved) != stored.spec_fingerprint
        || saved.is_finite_workload()
        || saved.kind != ComponentKind::Process
    {
        return Ok(Recovery::NotApplicable);
    }
    // Restart recovery is bound to the applied generation. The checkout may
    // have changed since apply; do not reject recovery or prove health against
    // the newer, unapplied declaration.
    target.specification = stored_deployment_specification(&row)?;
    let identity = stored.binding_identity.clone().ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "saved routed process identity is missing",
        )
    })?;
    let expected = process_unit_name(
        &deployments.config.deploy_unit_prefix(),
        deployment_id,
        &component_name,
        generation,
    );
    if stored.binding_kind.as_deref() != Some("unit") || identity != expected {
        return Ok(Recovery::NotApplicable);
    }
    let state = deployments
        .systemd
        .process_state(&identity)
        .map_err(systemd_error)?;
    if matches!(state.active_state.as_str(), "activating" | "deactivating") {
        return Ok(Recovery::Deferred);
    }
    let port = match route_port {
        Some(port) => port,
        None => deployments
            .database
            .call({
                let id = deployment_id.to_owned();
                let component = component_name.clone();
                move |connection| {
                    connection
                        .query_row(
                            "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=0",
                            rusqlite::params![id, component],
                            |row| row.get::<_, u16>(0),
                        )
                        .optional()
                        .map_err(DatabaseError::from)
                }
            })
            .map_err(database_error)?
            .ok_or_else(|| ProtocolError::new(ErrorCode::RouteLeaseConflict, "stable route lease is missing"))?,
    };
    let lease_matches = deployments
        .database
        .call({
            let id = deployment_id.to_owned();
            let component = component_name.clone();
            move |connection| {
                connection
                    .query_row(
                        "SELECT lease_id FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=0 AND port=?3 AND lease_id IS NOT NULL",
                        rusqlite::params![id, component, port],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            }
        })
        .map_err(database_error)?;
    let Some(stable_lease) = lease_matches else {
        return Ok(Recovery::NotApplicable);
    };
    if route_port.is_some() && route_lease.as_deref() != Some(stable_lease.as_str()) {
        return Ok(Recovery::NotApplicable);
    }
    if route_port.is_some() && state.active_state == "active" {
        // The normal validator will prove the existing listener and health;
        // avoid rewriting a healthy route or restarting its unit.
        return Ok(Recovery::NotApplicable);
    }
    if !deployments.port_availability.bindable(port)
        && !deployments.systemd.owns_tcp_listener(&identity, port)
    {
        return Ok(Recovery::NotApplicable);
    }
    let generation_path = deployments
        .store
        .generation(deployment_id, generation)?
        .map(|generation| generation.path)
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::DeploymentActionFailed,
                "saved generation path is missing",
            )
        })?;
    let environment_file = deployments
        .files
        .environment_path(deployment_id, &component_name, generation)
        .map_err(file_apply_error)?;
    let log_path = deployments
        .files
        .log_path(deployment_id, &component_name)
        .map_err(file_apply_error)?;
    let working_directory = generation_path
        .join(&saved.cwd)
        .canonicalize()
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::DeploymentActionFailed,
                "saved process working directory is unavailable",
            )
            .with_detail(error.to_string())
        })?;
    if !working_directory.starts_with(&generation_path) {
        return Ok(Recovery::NotApplicable);
    }
    if state.active_state != "active" {
        deployments
            .systemd
            .start_persistent(&PersistentUnitSpec {
                unit: identity.clone(),
                slice_name: deployment_slice(
                    &deployments.config.deploy_unit_prefix(),
                    deployment_id,
                ),
                uid: row.created_by_uid,
                gid: primary_gid(row.created_by_uid).map_err(systemd_error)?,
                working_directory,
                environment_file,
                command: saved.command.iter().map(OsString::from).collect(),
                log_path,
            })
            .map_err(|error| {
                apply_runtime_error("saved routed process failed to restart", error)
            })?;
    }
    let mut ports =
        crate::ports::assigned(&deployments.database, deployment_id, 0).map_err(runtime_error)?;
    ports.insert(component_name.clone(), port);
    let readiness = deployments.prove_health(
        &target,
        &saved,
        &("unit".into(), identity.clone()),
        &ports,
        generation,
    )?;
    if !readiness.ready {
        return Ok(Recovery::Deferred);
    }
    deployments.store.set_component_runtime(
        deployment_id,
        &component_name,
        ComponentRuntimePatch {
            desired_state: Some("running".into()),
            state: Some("running".into()),
            health: Some("healthy".into()),
            generation: Some(Some(generation)),
            binding_kind: Some(Some("unit".into())),
            binding_identity: Some(Some(identity)),
            last_error: Some(None),
            ..Default::default()
        },
    )?;
    deployments.store.set_route(
        Some(&domain),
        deployment_id,
        Some(&component_name),
        Some(port),
        Some(generation),
    )?;
    // A prior lease withdrawal marks the deployment degraded. Recompute from
    // every component so a recovered route clears that marker only when the
    // rest of the deployment is healthy too.
    deployments.recompute_state(&target)?;
    Ok(Recovery::Restored)
}
