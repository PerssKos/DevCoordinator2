use super::model::Context;
use super::*;
use crate::automation_test_support::{Fixture, FixtureClock};
use serde_json::json;
use std::path::PathBuf;

pub(super) fn record(id: &str, now: u64) -> Record {
    Record {
        update_sequence: 1,
        artifact: api::Artifact {
            artifact_id: id.into(),
            revision: 1,
            repository_id: Some("project-alpha".into()),
            repository_name: Some("Project Alpha".into()),
            group_id: None,
            group_name: None,
            accounting_id: String::new(),
            name: format!("Artifact {id}"),
            kind: api::Kind::BuildCache,
            effect: api::Effect::Rebuildable,
            ownership: "fixture".into(),
            filesystem_id: Some("filesystem-a".into()),
            allocated_bytes: Some(64),
            last_used_at_ms: None,
            observed_since_ms: 0,
            verified_at_ms: Some(now),
            eligible_at_ms: None,
            safety: api::Safety::NeedsReview,
            reasons: vec![],
            protected: false,
            deletable: false,
            automatic_eligible: false,
            dependencies: vec![],
            aliases: vec![],
            removed_at_ms: None,
        },
        locator: Locator::Directory {
            path: "/fixture".into(),
            device: 1,
            inode: 1,
            root: "/".into(),
            git_root: None,
        },
        resource_key: format!("resource-{id}"),
        ancestor_keys: vec![],
        fingerprint: "fixture".into(),
        owner_deployment: None,
        last_activity_signature: "fixture".into(),
        discovered_effect: api::Effect::Rebuildable,
        disposal_approved: false,
        disposal_reason: None,
        blockers: vec![],
        recovery_lineage: None,
        recovery_verified: false,
        recovery_created_at_ms: None,
        private_aliases: vec![],
        retention_eligible: false,
        ignored_review: None,
        source_owner_observation: None,
    }
}

pub(super) fn service(fixture: &Fixture) -> StorageService {
    StorageService::new(
        fixture.config.clone(),
        fixture.database.clone(),
        Arc::new(FixtureClock),
        CapacityBroker::new(fixture.database.clone(), fixture.config.socket_path.clone()).unwrap(),
        EventService::new(fixture.database.clone()).unwrap(),
    )
}

pub(super) fn save(service: &StorageService, record: &Record) {
    let encoded = json(record).unwrap();
    let at = service.now_ms();
    service.database.call(|connection|{
        connection.execute("INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(artifact_id) DO UPDATE SET record_json=excluded.record_json,revision=excluded.revision,removed_at_ms=excluded.removed_at_ms",crate::database::params![record.artifact.artifact_id,record.artifact.repository_id,atom(record.artifact.kind),record.artifact.revision,record.artifact.removed_at_ms,encoded,at])?;
        Ok(())
    }).unwrap();
}

#[test]
fn selected_candidate_origins_block_worktree_cleanup_without_pinning_history_forever() {
    let fixture = Fixture::new();
    let service = service(&fixture);
    let main = &fixture.repository;
    let temporary = fixture._temporary.path();
    let remote = temporary.join("origin.git");
    let git = |path: &std::path::Path, args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .current_dir(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    };
    git(
        temporary,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            remote.to_str().unwrap(),
        ],
    );
    git(main, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(main.join(".devcoordinator.toml"), "schema=2\n[deployment.web]\nsource=\"checkout\"\ncomponents=[\"web\"]\n[deployment.web.component.web]\ntype=\"process\"\ncommand=[\"serve\"]\n").unwrap();
    git(main, &["add", ".devcoordinator.toml"]);
    git(
        main,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Cleanup fixture",
        ],
    );
    git(main, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(main, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
    let origins = [
        temporary.join("previous-origin"),
        temporary.join("current-origin"),
        temporary.join("historical-origin"),
    ];
    for path in &origins {
        git(
            main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
            ],
        );
    }
    let uid = unsafe { libc::geteuid() };
    let spec = crate::repository_config::load_deployment_spec(main, "web")
        .unwrap()
        .canonical("checkout");
    let copied_origins = origins.clone();
    fixture.database.transaction(move |db| {
        db.execute("UPDATE repositories SET registered_by_uid=$1 WHERE repository_id='project-alpha'",[uid])?;
        db.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at) VALUES('d1111111111111111','project-alpha','w1111111111111111','web','checkout','fixture',$1,'running',2,1,'t',$2,'fixture','t')", crate::database::params![spec.to_string(),uid])?;
        for (index, path) in copied_origins.iter().enumerate() {
            let number=index as u32+1;
            db.execute("INSERT INTO generations(deployment_id,number,commit_hash,dirty,path,fingerprint,created_at,state) VALUES('d1111111111111111',$1,'fixture',0,$2,'fixture','t','current')",crate::database::params![number,temporary.join(format!("immutable-{number}")).to_string_lossy().into_owned()])?;
            db.execute("INSERT INTO generation_source_origins(deployment_id,number,worktree_id,path) VALUES('d1111111111111111',$1,$2,$3)", crate::database::params![number,crate::ids::worktree_id(path).unwrap(),path.to_string_lossy().into_owned()])?;
        }
        Ok(())
    }).unwrap();
    let backend = native::HostBackend::new(fixture.config.clone(), fixture.database.clone());
    let candidate = |path: &std::path::Path| {
        let mut row = record("candidate-origin", service.now_ms());
        let (device, inode) = fs::identity(path).unwrap();
        row.artifact.kind = api::Kind::Worktree;
        row.artifact.effect = api::Effect::SourceWorktree;
        row.locator = Locator::Directory {
            path: path.into(),
            device,
            inode,
            root: temporary.into(),
            git_root: Some(main.clone()),
        };
        row
    };
    let released = |path: &std::path::Path, context: &Context| {
        if let Err(error) = native::Backend::validate(&backend, &candidate(path), context, &[]) {
            // Non-root checks cannot inspect unrelated root-owned /proc entries.
            // This existing later guard may remain unavailable, but any cleanup
            // protection refusal, including current_deployment, must fail here.
            assert_eq!(error.code, ErrorCode::StorageUnavailable);
            assert_eq!(error.message, "process_inventory_incomplete");
        }
    };
    let context = service.context().unwrap();
    for path in &origins[..2] {
        assert!(context.current_paths.contains(path));
        let error =
            native::Backend::validate(&backend, &candidate(path), &context, &[]).unwrap_err();
        assert!(error.message.contains("current_deployment"));
    }
    assert!(!context.current_paths.contains(&origins[2]));
    released(&origins[2], &context);
    // Removing the old checkout's declaration does not retire the actual
    // selected candidate/rollback source references.
    std::fs::write(main.join(".devcoordinator.toml"), "schema=2\n").unwrap();
    let context = service.context().unwrap();
    assert!(context.current_paths.contains(&origins[0]));
    assert!(context.current_paths.contains(&origins[1]));
    fixture.database.transaction(|db| {
        db.execute("UPDATE deployments SET current_generation=3,previous_generation=NULL WHERE deployment_id='d1111111111111111'",[])?;
        Ok(())
    }).unwrap();
    crate::deployment_state::DeploymentStore::new(fixture.database.clone())
        .prune_generations("d1111111111111111", &std::collections::BTreeSet::from([3]))
        .unwrap();
    let context = service.context().unwrap();
    for path in &origins[..2] {
        assert!(!context.current_paths.contains(path));
        released(path, &context);
    }
    assert!(context.current_paths.contains(&origins[2]));
    assert!(
        native::Backend::validate(&backend, &candidate(&origins[2]), &context, &[])
            .unwrap_err()
            .message
            .contains("current_deployment")
    );
}

#[test]
fn native_storage_pages_match_current_safety_without_loading_the_catalog() {
    let fixture = Fixture::new();
    let service = service(&fixture);
    let now = service.now_ms();
    let mut records = (0..28)
        .map(|index| record(&format!("artifact-{index:02}"), now))
        .collect::<Vec<_>>();
    records[0].artifact.name = "ÉCHO cache".into();
    records[1].artifact.protected = true;
    records[2].blockers = vec!["active_consumer".into()];
    records[2].artifact.name = "İstanbul cache".into();
    records[3].artifact.verified_at_ms = None;
    records[4].artifact.verified_at_ms = Some(now - OBSERVATION_FRESH_MS - 1);
    records[5].artifact.verified_at_ms = Some(now - OBSERVATION_FRESH_MS);
    records[6].artifact.verified_at_ms = Some(u64::MAX);
    records[7].blockers = vec!["evidence_retention_pending".into()];
    records[8].discovered_effect = api::Effect::Unknown;
    records[9].artifact.dependencies = vec![records[1].artifact.artifact_id.clone()];
    records[10].artifact.dependencies = vec![records[11].artifact.artifact_id.clone()];
    records[11].artifact.dependencies = vec![records[10].artifact.artifact_id.clone()];
    records[12].artifact.dependencies = vec!["missing-dependency".into()];
    records[13].artifact.dependencies = vec![
        records[2].artifact.artifact_id.clone(),
        records[1].artifact.artifact_id.clone(),
    ];
    records[14].artifact.dependencies = vec![
        records[15].artifact.artifact_id.clone(),
        records[1].artifact.artifact_id.clone(),
    ];
    records[15].artifact.dependencies = vec![records[2].artifact.artifact_id.clone()];
    records[16].artifact.removed_at_ms = Some(now - 100);
    records[16].artifact.protected = true;
    records[17].artifact.dependencies = vec![records[16].artifact.artifact_id.clone()];
    records[18].ancestor_keys = vec!["shared-parent".into()];
    records[18].artifact.protected = true;
    records[19].resource_key = "shared-parent".into();
    records[20].ancestor_keys = vec!["shared-parent".into()];
    records[21].artifact.repository_id = None;
    records[21].artifact.repository_name = None;
    records[22].resource_key = records[23].resource_key.clone();
    // The native SQL page/filter and the Rust classifier must preserve the
    // existing fail-closed source-owner annotation, including malformed bindings.
    for record in &mut records[24..] {
        let root = PathBuf::from("/fixture-repository");
        if let Locator::Directory { git_root, .. } = &mut record.locator {
            *git_root = Some(root.clone());
        }
        record.source_owner_observation = Some(model::SourceOwnerObservation::Unavailable {
            resource_key: record.resource_key.clone(),
            fingerprint: record.fingerprint.clone(),
            git_root: root,
            origin: model::SourceOwnerOrigin::Registered,
        });
    }
    records[25].fingerprint = "changed-binding".into();
    records[26].blockers = vec!["tracked_source".into()];
    records[27].artifact.protected = true;
    for record in &records {
        save(&service, record);
    }
    fixture.database.call(|connection|{
        connection.execute("INSERT INTO storage_leases(lease_id,actor,lease_json,expires_at_ms) VALUES('active','fixture',$1,$2)",crate::database::params![json!({"lease_id":"active","artifact_ids":[records[22].artifact.artifact_id],"expires_at_ms":u64::MAX}).to_string(),now+1000])?;
        connection.execute("INSERT INTO storage_leases(lease_id,actor,lease_json,expires_at_ms) VALUES('expired','fixture',$1,$2)",crate::database::params![json!({"lease_id":"expired","artifact_ids":[records[0].artifact.artifact_id],"expires_at_ms":now-1000}).to_string(),now-1000])?;
        Ok(())
    }).unwrap();
    let expected = service.projected_records().unwrap();
    assert!(
        expected[&records[24].artifact.artifact_id]
            .reasons
            .contains(&"git_execution_owner_unverified".into())
    );
    assert!(
        expected[&records[25].artifact.artifact_id]
            .reasons
            .contains(&"stored_storage_record_invalid".into())
    );
    let rechecked = service
        .base_artifacts(
            &records[24..]
                .iter()
                .map(|record| {
                    (
                        record.artifact.artifact_id.clone(),
                        record.for_caller_validation(),
                    )
                })
                .collect(),
        )
        .unwrap()
        .0;
    assert!(rechecked[&records[24].artifact.artifact_id].deletable);
    for record in &records[25..] {
        assert!(!rechecked[&record.artifact.artifact_id].deletable);
    }
    let projection = service.projection().unwrap();
    let eligibility_sql = format!(
        "{} SELECT artifact_id,eligible_at_ms::text,automatic AND base_safety='safe' AND removed_at_ms IS NULL AND eligible_at_ms<=$1::bigint::numeric FROM eligibility",
        storage_eligibility_projection()
    );
    let eligibility = fixture
        .database
        .read(|connection| {
            let mut query = connection.prepare(&eligibility_sql)?;
            Ok(query
                .query_map([now], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<bool>>(2)?.unwrap_or(false),
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    for (id, at, due) in eligibility {
        let projected = service
            .project_with(service.record(&id).unwrap(), &projection)
            .unwrap();
        assert_eq!(
            at.map(|value| value.parse::<u64>().unwrap()),
            projected.eligible_at_ms,
            "eligibility timestamp for {id}"
        );
        assert_eq!(
            due, projected.automatic_eligible,
            "automatic eligibility for {id}"
        );
    }
    assert_eq!(
        expected[&records[10].artifact.artifact_id].safety,
        api::Safety::Safe
    );
    assert_eq!(
        expected[&records[13].artifact.artifact_id].safety,
        api::Safety::InUse
    );
    assert_eq!(
        expected[&records[14].artifact.artifact_id].safety,
        api::Safety::Protected
    );
    assert_eq!(
        expected[&records[19].artifact.artifact_id].safety,
        api::Safety::Protected
    );
    assert_eq!(
        expected[&records[20].artifact.artifact_id].safety,
        api::Safety::Safe
    );
    for (id, artifact) in &expected {
        assert_eq!(
            &service.artifact(id).unwrap(),
            artifact,
            "native classifier differs for {id}"
        );
    }
    assert_eq!(
        service.artifact("not-present").unwrap_err().code,
        ErrorCode::StorageNotFound
    );
    let mut ordered = expected
        .values()
        .filter(|value| value.removed_at_ms.is_none())
        .cloned()
        .collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        a.repository_name
            .cmp(&b.repository_name)
            .then(a.name.cmp(&b.name))
            .then(a.artifact_id.cmp(&b.artifact_id))
    });
    let mut offset = 0;
    let mut got = Vec::new();
    loop {
        let page = service
            .inventory(api::List {
                offset,
                limit: Some(3),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, ordered.len() as u64);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 192 * 1024);
        got.extend(page.artifacts);
        let Some(next) = page.next_offset else { break };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(got, ordered);
    let matching = service
        .inventory(api::List {
            query: Some("éCHO".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(matching.total, 1);
    assert_eq!(
        matching.artifacts[0].artifact_id,
        records[0].artifact.artifact_id
    );
    let matching = service
        .inventory(api::List {
            query: Some("i\u{307}STANBUL".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(matching.total, 1);
    assert_eq!(
        matching.artifacts[0].artifact_id,
        records[2].artifact.artifact_id
    );
    for safety in [
        api::Safety::Safe,
        api::Safety::Protected,
        api::Safety::InUse,
        api::Safety::NeedsReview,
        api::Safety::Observing,
    ] {
        let page = service
            .inventory(api::List {
                safety: Some(safety),
                include_removed: true,
                limit: Some(100),
                ..Default::default()
            })
            .unwrap();
        let want = expected
            .values()
            .filter(|value| value.safety == safety)
            .count();
        assert_eq!(page.total, want as u64);
        assert_eq!(page.artifacts.len(), want);
    }
    // Pins, leases, and expired observations are read from their current
    // authority rows, without waiting for another filesystem discovery.
    let mut changed = records[1].clone();
    changed.artifact.protected = false;
    save(&service, &changed);
    let expected = service.projected_records().unwrap();
    for id in [
        &records[1].artifact.artifact_id,
        &records[9].artifact.artifact_id,
    ] {
        assert_eq!(service.artifact(id).unwrap(), expected[id]);
    }

    let caller = crate::access::Caller {
        via_edge: false,
        pid: 1,
        uid: 1000,
        gid: 1000,
        client_kind: devcoordinator2_api::ClientKind::Codex,
        model: None,
        effort: None,
        client_session: None,
        work: None,
        identity: None,
    };
    let data = fixture
        .database
        .data_directory()
        .expect("fixture PostgreSQL binding");
    for protected in [data, data.parent().unwrap()] {
        let error = service
            .set_root(
                api::RootSet {
                    root_id: None,
                    repository_id: Some("project-alpha".into()),
                    expected_revision: 0,
                    label: "database cannot be disposable".into(),
                    path: protected.to_string_lossy().into_owned(),
                    kind: api::Kind::BuildOutput,
                },
                &caller,
            )
            .unwrap_err();
        assert!(matches!(
            error.message.as_str(),
            "protected_system_data" | "protected_root"
        ));
    }
    assert!(service.roots().unwrap().roots.is_empty());
    let cutover = fixture.config.state_dir.join("cutover");
    std::fs::create_dir_all(&cutover).unwrap();
    for (path, kind) in [
        (fixture.config.state_dir.clone(), api::Kind::Backup),
        (cutover.clone(), api::Kind::DependencyCache),
        (cutover.join("generation"), api::Kind::Backup),
    ] {
        assert_eq!(
            service
                .set_root(
                    api::RootSet {
                        root_id: None,
                        repository_id: Some("project-alpha".into()),
                        expected_revision: 0,
                        label: "Protected native authority".into(),
                        path: path.to_string_lossy().into_owned(),
                        kind
                    },
                    &caller
                )
                .unwrap_err()
                .message,
            "protected_system_data"
        );
    }
    let root = service
        .set_root(
            api::RootSet {
                root_id: None,
                repository_id: Some("project-alpha".into()),
                expected_revision: 0,
                label: "Native recovery generations".into(),
                path: cutover.to_string_lossy().into_owned(),
                kind: api::Kind::Backup,
            },
            &caller,
        )
        .unwrap();
    assert_eq!(root.kind, api::Kind::Backup);
    let plan = |id: &str| {
        service.plan(
            api::PlanRequest {
                artifact_ids: vec![id.into()],
                automatic: false,
                include_persistent_data: true,
            },
            &caller,
        )
    };
    assert_eq!(
        plan(&records[10].artifact.artifact_id).unwrap_err().message,
        "dependency_cycle"
    );
    assert_eq!(
        plan(&records[12].artifact.artifact_id).unwrap_err().message,
        "dependency_not_found"
    );
    let mut volume = record("graph-volume", now);
    volume.artifact.kind = api::Kind::Volume;
    volume.resource_key = "graph-physical".into();
    volume.locator = Locator::Docker {
        object_type: "volume".into(),
        identity: "graph-volume".into(),
        created: "fixture".into(),
    };
    let mut backing = record("graph-backing", now);
    backing.artifact.kind = api::Kind::BackingDirectory;
    backing.resource_key = volume.resource_key.clone();
    backing.artifact.dependencies = vec![volume.artifact.artifact_id.clone()];
    let mut child = record("graph-child", now);
    child.ancestor_keys = vec![backing.resource_key.clone()];
    for value in [&volume, &backing, &child] {
        save(&service, value);
    }
    let result = plan(&volume.artifact.artifact_id).unwrap();
    // These synthetic locators prove graph expansion, not actual removable
    // host resources. The caller-bound preflight must keep them non-ready.
    assert!(!result.ready);
    assert_eq!(
        result
            .items
            .into_iter()
            .map(|item| item.artifact_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            volume.artifact.artifact_id,
            backing.artifact.artifact_id,
            child.artifact.artifact_id
        ])
    );

    // Registration historically updates JSON ownership before another scan
    // repairs the old scalar column. Reads must follow the recorded owner.
    let mut assigned = records[21].clone();
    assigned.artifact.repository_id = Some("project-alpha".into());
    assigned.artifact.repository_name = Some("Project Alpha".into());
    save(&service, &assigned);
    let stale = fixture
        .database
        .read(|connection| {
            connection
                .query_row(
                    "SELECT repository_id FROM storage_artifacts WHERE artifact_id=$1",
                    [&assigned.artifact.artifact_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .map_err(DatabaseError::from)
        })
        .unwrap();
    assert!(stale.is_none());
    let scoped = service
        .inventory(api::List {
            repository_id: Some("project-alpha".into()),
            limit: Some(100),
            ..Default::default()
        })
        .unwrap();
    assert!(
        scoped
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_id == assigned.artifact.artifact_id)
    );
    service
        .set_policy(
            api::PolicySet {
                repository_id: Some("project-alpha".into()),
                expected_revision: 0,
                automatic: false,
                cache_idle_seconds: api::CACHE_IDLE_SECONDS,
                data_idle_seconds: api::DATA_IDLE_SECONDS,
                minimum_verified_backups: 2,
            },
            &caller,
        )
        .unwrap();
    let sql = format!(
        "{} SELECT automatic FROM eligibility WHERE artifact_id=$2",
        storage_eligibility_projection()
    );
    let automatic = fixture
        .database
        .read(|connection| {
            connection
                .query_row(
                    &sql,
                    crate::database::params![now, assigned.artifact.artifact_id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(DatabaseError::from)
        })
        .unwrap();
    assert!(!automatic);

    // A scan can publish a live-sized catalog before ANALYZE or VACUUM runs.
    // Exercise metadata paging and every safety filter against those fresh
    // writes, with exact totals and no test-only index maintenance.
    let baseline = service.projected_records().unwrap();
    let mut seed = record("catalog-seed", now);
    seed.artifact.repository_id = Some("catalog-project".into());
    seed.artifact.repository_name = Some("Catalog Project".into());
    let encoded = json(&seed).unwrap();
    for first in (0_i64..25_000).step_by(500) {
        let last = (first + 499).min(24_999);
        fixture.database.transaction(|connection| {
            connection.execute("INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms)
            SELECT 'catalog-'||lpad(n::text,5,'0'),'catalog-project','build_cache',1,NULL,
              jsonb_set(jsonb_set(jsonb_set(jsonb_set(jsonb_set($1::jsonb,
                '{artifact,artifact_id}',to_jsonb('catalog-'||lpad(n::text,5,'0'))),
                '{artifact,name}',to_jsonb('Catalog '||lpad(n::text,5,'0'))),
                '{artifact,protected}',to_jsonb(n%5=1)),
                '{resource_key}',to_jsonb('catalog-resource-'||n::text)),
                '{blockers}',CASE n%5 WHEN 2 THEN '[\"active_consumer\"]'::jsonb WHEN 3 THEN '[\"ownership_unknown\"]'::jsonb WHEN 4 THEN '[\"evidence_retention_pending\"]'::jsonb ELSE '[]'::jsonb END),$2
            FROM generate_series($3::bigint,$4::bigint) n",crate::database::params![encoded,now,first,last])?;
            Ok(())
        }).unwrap();
    }
    for safety in [
        None,
        Some(api::Safety::Safe),
        Some(api::Safety::Protected),
        Some(api::Safety::InUse),
        Some(api::Safety::NeedsReview),
        Some(api::Safety::Observing),
    ] {
        let started = std::time::Instant::now();
        let page = service
            .inventory(api::List {
                repository_id: Some("catalog-project".into()),
                safety,
                limit: Some(50),
                ..Default::default()
            })
            .unwrap();
        eprintln!(
            "storage catalog filter {safety:?}: {} ms",
            started.elapsed().as_millis()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "catalog safety filter {safety:?} exceeded the bounded fixture budget"
        );
        assert_eq!(page.total, if safety.is_some() { 5_000 } else { 25_000 });
        assert_eq!(page.artifacts.len(), 50);
        assert_eq!(page.next_offset, Some(50));
        if let Some(safety) = safety {
            assert!(
                page.artifacts
                    .iter()
                    .all(|artifact| artifact.safety == safety)
            );
        }
    }
    let started = std::time::Instant::now();
    let page = service
        .inventory(api::List {
            limit: Some(50),
            ..Default::default()
        })
        .unwrap();
    eprintln!(
        "storage catalog default: {} ms",
        started.elapsed().as_millis()
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert_eq!(
        page.total,
        25_000
            + baseline
                .values()
                .filter(|a| a.removed_at_ms.is_none())
                .count() as u64
    );
}

#[test]
fn bounded_worktree_intent_preserves_blockers_and_live_safety_guards() {
    let fixture = Fixture::new();
    let service = service(&fixture);
    let now = service.now_ms();
    let worktree = fixture._temporary.path().join("linked-worktree");
    let protected_alias = fixture._temporary.path().join("protected-alias");
    let leased_descendant = worktree.join("leased-descendant");
    for path in [&worktree, &protected_alias, &leased_descendant] {
        std::fs::create_dir_all(path).unwrap();
    }
    let worktree_identity = fs::identity(&worktree).unwrap();
    let mut target = record("bounded-worktree", now);
    target.artifact.kind = api::Kind::Worktree;
    target.artifact.effect = api::Effect::SourceWorktree;
    target.artifact.ownership = "recognized".into();
    target.artifact.repository_id = Some("project-alpha".into());
    target.artifact.repository_name = Some("Project Alpha".into());
    target.artifact.protected = false;
    target.artifact.deletable = false;
    target.locator = Locator::Directory {
        path: worktree.clone(),
        device: worktree_identity.0,
        inode: worktree_identity.1,
        root: worktree.clone(),
        git_root: Some(fixture.repository.clone()),
    };
    target.resource_key = "worktree-resource".into();
    target.ancestor_keys = vec!["protected-alias-resource".into()];
    target.discovered_effect = api::Effect::SourceWorktree;
    target.blockers = vec!["dirty_worktree".into()];
    target.private_aliases = vec![worktree.clone()];

    let mut protected = record("protected-alias", now);
    protected.artifact.protected = true;
    protected.resource_key = "protected-alias-resource".into();
    protected.locator = Locator::Directory {
        path: protected_alias.clone(),
        device: fs::identity(&protected_alias).unwrap().0,
        inode: fs::identity(&protected_alias).unwrap().1,
        root: protected_alias.clone(),
        git_root: None,
    };
    protected.private_aliases = vec![protected_alias];

    let mut descendant = record("leased-descendant", now);
    descendant.resource_key = "leased-descendant-resource".into();
    descendant.ancestor_keys = vec![target.resource_key.clone()];
    descendant.locator = Locator::Directory {
        path: leased_descendant.clone(),
        device: fs::identity(&leased_descendant).unwrap().0,
        inode: fs::identity(&leased_descendant).unwrap().1,
        root: worktree,
        git_root: None,
    };
    descendant.private_aliases = vec![leased_descendant];
    save(&service, &target);
    save(&service, &protected);
    save(&service, &descendant);
    let backend = native::HostBackend::new(fixture.config.clone(), fixture.database.clone());
    let error =
        native::Backend::validate_disposal_intent(&backend, &target, "project-alpha", now, None)
            .unwrap_err();
    assert_eq!(error.message, "protected_shared_data");

    protected.artifact.protected = false;
    save(&service, &protected);
    fixture
        .database
        .call(|connection| {
            connection.execute(
                "INSERT INTO storage_leases(lease_id,actor,lease_json,expires_at_ms) VALUES('bounded-lease','fixture',$1,$2)",
                crate::database::params![
                    json!({
                        "lease_id":"bounded-lease",
                        "artifact_ids":["leased-descendant"],
                        "expires_at_ms":now+60_000
                    })
                    .to_string(),
                    (now + 60_000) as i64,
                ],
            )?;
            Ok(())
        })
        .unwrap();
    let error =
        native::Backend::validate_disposal_intent(&backend, &target, "project-alpha", now, None)
            .unwrap_err();
    assert_eq!(error.message, "active_lease");
    fixture
        .database
        .call(|connection| {
            connection.execute(
                "DELETE FROM storage_leases WHERE lease_id='bounded-lease'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    let caller = crate::access::Caller {
        via_edge: false,
        pid: 1,
        uid: 1000,
        gid: 1000,
        client_kind: devcoordinator2_api::ClientKind::Codex,
        model: None,
        effort: None,
        client_session: None,
        work: None,
        identity: None,
    };
    let result = service
        .register(
            api::Register {
                artifact_id: target.artifact.artifact_id.clone(),
                expected_revision: target.artifact.revision,
                repository_id: Some("project-alpha".into()),
                effect: api::Effect::SourceWorktree,
                reviewed_ignored_files: None,
                reason: "Preserve source-worktree blockers".into(),
            },
            &caller,
        )
        .unwrap();
    assert!(!result.deletable);
    assert!(
        result
            .reasons
            .iter()
            .any(|reason| reason == "dirty_worktree")
    );
    let saved = service.record("bounded-worktree").unwrap();
    assert!(saved.disposal_approved);
    assert!(
        saved
            .blockers
            .iter()
            .any(|blocker| blocker == "dirty_worktree")
    );
}
