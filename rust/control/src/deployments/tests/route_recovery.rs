use super::*;

#[path = "wait.rs"]
mod wait;

#[test]
fn explicit_linked_candidate_status_tracks_its_origin_by_deployment_id() {
    let fixture = RouteWorld::new("checkout");
    for args in [
        vec!["add", ".devcoordinator.toml"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Fixture source",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&fixture.world.worktree)
                .status()
                .unwrap()
                .success()
        );
    }
    let linked = fixture.world._temporary.path().join("linked-candidate");
    assert!(
        std::process::Command::new("git")
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&linked)
            .current_dir(&fixture.world.worktree)
            .status()
            .unwrap()
            .success()
    );
    fixture.git.snapshots.lock().unwrap().insert(
        linked.clone(),
        crate::deployment_git::GitSnapshot {
            commit: Some("b".repeat(40)),
            dirty: false,
            source_digest: "b".repeat(64),
        },
    );
    let first = fixture.apply().unwrap();
    let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
        path: linked.to_string_lossy().into_owned(),
        commit: "b".repeat(40),
    };
    let declaration = fixture.world.worktree.join(".devcoordinator.toml");
    let original = std::fs::read(&declaration).unwrap();
    std::fs::write(&declaration, "schema = [\n").unwrap();
    let preflight = fixture.deployments.preflight(
        None,
        None,
        Some(&first.deployment_id),
        Some(&candidate),
        &fixture.caller,
    );
    let applied = fixture.deployments.apply_with_candidate(
        None,
        None,
        Some(&first.deployment_id),
        Some(&candidate),
        &fixture.caller,
    );
    std::fs::write(&declaration, &original).unwrap();
    eprintln!(
        "malformed original declaration: candidate preflight={:?}, apply={:?}",
        preflight
            .as_ref()
            .map(|result| result.ready)
            .map_err(|error| error.code),
        applied
            .as_ref()
            .map(|result| result.current_generation)
            .map_err(|error| error.code)
    );
    assert!(
        preflight.as_ref().is_ok_and(|result| result.ready),
        "clean candidate preflight must read its own declaration"
    );
    let applied = applied.expect("clean candidate apply must read its own declaration");
    assert!(applied.readiness.unwrap().ready);
    assert!(
        fixture
            .status(&first.deployment_id)
            .readiness
            .unwrap()
            .ready,
        "status by ID must compare the selected candidate origin, not the older declared checkout"
    );
    let id = &first.deployment_id;
    let stored = fixture.deployments.store.get(id).unwrap().unwrap();
    let origin = fixture
        .deployments
        .store
        .generation_origin(id, 2)
        .unwrap()
        .unwrap();
    assert_eq!(origin.path, linked);
    assert_ne!(stored.worktree_id, origin.worktree_id);
    assert!(
        fixture
            .reopen()
            .status(None, None, Some(id), &fixture.caller)
            .unwrap()
            .readiness
            .unwrap()
            .ready
    );
    assert!(
        fixture
            .deployments
            .status(
                Some(fixture.world.worktree.to_str().unwrap()),
                Some("web"),
                None,
                &fixture.caller
            )
            .unwrap()
            .readiness
            .unwrap()
            .ready
    );
    fixture.git.snapshot.lock().unwrap().dirty = true;
    assert!(
        fixture.status(id).readiness.unwrap().ready,
        "edits to the declared checkout must not rebind a candidate"
    );
    fixture
        .git
        .snapshots
        .lock()
        .unwrap()
        .get_mut(&linked)
        .unwrap()
        .dirty = true;
    assert_eq!(
        fixture.status(id).readiness.unwrap().pending_apply,
        Some(true)
    );
    assert!(
        fixture
            .deployments
            .apply_with_candidate(None, None, Some(id), Some(&candidate), &fixture.caller)
            .is_err()
    );
    assert_eq!(fixture.status(id).current_generation, Some(2));
    fixture
        .git
        .snapshots
        .lock()
        .unwrap()
        .get_mut(&linked)
        .unwrap()
        .dirty = false;
    let missing = linked.with_extension("temporarily-moved");
    std::fs::rename(&linked, &missing).unwrap();
    let unavailable = fixture.status(id).readiness.unwrap();
    assert!(!unavailable.ready);
    assert_eq!(unavailable.source_state, "unknown");
    assert!(!unavailable.blockers.is_empty());
    std::fs::rename(&missing, &linked).unwrap();
    fixture.git.snapshot.lock().unwrap().dirty = false;
    let ordinary = fixture
        .deployments
        .apply(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert_eq!(ordinary.current_generation, Some(3));
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, 3)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture.deployments.store.generation_origin(id, 2).unwrap(),
        Some(origin)
    );
    fixture.git.snapshot.lock().unwrap().dirty = true;
    assert_eq!(
        fixture.status(id).readiness.unwrap().pending_apply,
        Some(true)
    );
}

#[test]
fn explicit_worktree_candidate_keeps_identity_and_tracks_its_origin() {
    let fixture = RouteWorld::new("worktree");
    let first = fixture.apply().unwrap();
    fixture.change();
    let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
        path: fixture.world.worktree.to_string_lossy().into_owned(),
        commit: "b".repeat(40),
    };
    let preflight = fixture
        .deployments
        .preflight(
            None,
            None,
            Some(&first.deployment_id),
            Some(&candidate),
            &fixture.caller,
        )
        .unwrap();
    assert!(preflight.ready);
    fixture.git.snapshot.lock().unwrap().dirty = true;
    let blocked = fixture
        .deployments
        .preflight(
            None,
            None,
            Some(&first.deployment_id),
            Some(&candidate),
            &fixture.caller,
        )
        .unwrap();
    assert!(!blocked.ready);
    assert!(
        blocked
            .blockers
            .iter()
            .any(|blocker| blocker.message.contains("candidate source"))
    );
    fixture.git.snapshot.lock().unwrap().dirty = false;
    let applied = fixture
        .deployments
        .apply_with_candidate(
            None,
            None,
            Some(&first.deployment_id),
            Some(&candidate),
            &fixture.caller,
        )
        .unwrap();
    assert_eq!(applied.deployment_id, first.deployment_id);
    assert_eq!(applied.current_generation, Some(2));
    assert!(applied.readiness.unwrap().ready);
    let retained = fixture
        .deployments
        .store
        .generation(&first.deployment_id, 2)
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.path,
        fixture
            .deployments
            .files
            .generation_path(&first.deployment_id, 2)
            .unwrap(),
        "explicit worktree candidate must run from its retained generation"
    );
    let origin = fixture
        .deployments
        .store
        .generation_origin(&first.deployment_id, 2)
        .unwrap()
        .unwrap();
    assert_eq!(origin.path, fixture.world.worktree);
    assert!(
        fixture
            .status(&first.deployment_id)
            .readiness
            .unwrap()
            .ready
    );
    assert_eq!(
        applied.previous_generation, None,
        "mutable original source is not a frozen rollback target"
    );
    let id = first.deployment_id.clone();
    let legacy_path = fixture.world.worktree.to_string_lossy().into_owned();
    fixture
        .world
        .database
        .transaction(move |db| {
            db.execute(
                "UPDATE generations SET path=$1 WHERE deployment_id=$2 AND number=2",
                crate::database::params![legacy_path, id],
            )?;
            Ok(())
        })
        .unwrap();
    let upgraded = fixture
        .deployments
        .apply_with_candidate(
            None,
            None,
            Some(&first.deployment_id),
            Some(&candidate),
            &fixture.caller,
        )
        .unwrap();
    assert_eq!(
        upgraded.current_generation,
        Some(3),
        "legacy mutable candidate must be frozen even when its source is unchanged"
    );
    assert_ne!(upgraded.unchanged, Some(true));
    let frozen = fixture
        .deployments
        .store
        .generation(&first.deployment_id, 3)
        .unwrap()
        .unwrap()
        .path;
    let mutable = fixture.apply().unwrap();
    assert_eq!(mutable.previous_generation, Some(3));
    assert_eq!(
        fixture
            .deployments
            .store
            .generation(&first.deployment_id, 4)
            .unwrap()
            .unwrap()
            .path,
        fixture.world.worktree,
        "ordinary worktree apply must keep developer editing semantics"
    );
    let restored = fixture
        .deployments
        .rollback(None, None, Some(&first.deployment_id), &fixture.caller)
        .unwrap();
    assert_eq!(restored.rolled_back_to, Some(3));
    assert_eq!(
        fixture
            .deployments
            .store
            .generation(&first.deployment_id, 5)
            .unwrap()
            .unwrap()
            .path,
        frozen,
        "rollback must keep the exact frozen source"
    );
    assert!(fixture.world.worktree.is_dir());

    let declaration = fixture.world.worktree.join(".devcoordinator.toml");
    let original = std::fs::read(&declaration).unwrap();
    let mut failures = Vec::new();
    for (state, bytes) in [("malformed", Some("schema = [\n")), ("removed", None)] {
        if let Some(bytes) = bytes {
            std::fs::write(&declaration, bytes).unwrap();
        } else {
            std::fs::remove_file(&declaration).unwrap();
        }
        for action in ["stop", "start", "restart"] {
            match fixture.deployments.control(
                action,
                None,
                None,
                Some(&first.deployment_id),
                None,
                &fixture.caller,
            ) {
                Ok(status) => {
                    assert_eq!(status.current_generation, restored.current_generation);
                    if action != "stop" {
                        assert_eq!(status.route_port, restored.route_port);
                    }
                }
                Err(error) => {
                    failures.push(format!("{action} with {state} declaration: {}", error.code))
                }
            }
        }
        assert_eq!(
            fixture
                .deployments
                .store
                .generation(&first.deployment_id, 5)
                .unwrap()
                .unwrap()
                .path,
            frozen
        );
        std::fs::write(&declaration, &original).unwrap();
    }
    assert!(
        failures.is_empty(),
        "saved runtime controls depend on mutable source: {failures:?}"
    );
}

#[test]
fn historical_process_logs_never_substitute_another_generation() {
    let fixture = RouteWorld::new("worktree");
    let first = fixture.apply().unwrap();
    let legacy = fixture
        .deployments
        .files
        .log_path(&first.deployment_id, "api")
        .unwrap();
    std::fs::write(legacy, "unrelated legacy startup\n").unwrap();
    let logs = fixture
        .deployments
        .logs(
            DeploymentLogsParams {
                attempt: None,
                deployment_id: Some(first.deployment_id.clone()),
                path: None,
                name: None,
                component: "api".into(),
                generation: Some(99),
                tail_lines: 10,
            },
            &fixture.caller,
        )
        .unwrap();
    assert!(
        logs.tail.is_empty(),
        "historical log substituted legacy output"
    );
    let path = fixture
        .deployments
        .files
        .reserve_process_log(
            &first.deployment_id,
            "removed",
            7,
            fixture.caller.uid,
            fixture.caller.gid,
        )
        .unwrap();
    std::fs::write(path, "removed component startup\n").unwrap();
    let retained = fixture
        .deployments
        .logs(
            DeploymentLogsParams {
                deployment_id: Some(first.deployment_id.clone()),
                path: None,
                name: None,
                component: "removed".into(),
                generation: Some(7),
                attempt: None,
                tail_lines: 10,
            },
            &fixture.caller,
        )
        .unwrap();
    assert_eq!(retained.tail, "removed component startup");
    assert_eq!(retained.process_log.unwrap().generation, Some(7));
    let invalid = fixture
        .deployments
        .logs(
            DeploymentLogsParams {
                deployment_id: Some(first.deployment_id.clone()),
                path: None,
                name: None,
                component: "api".into(),
                generation: None,
                attempt: Some(1),
                tail_lines: 10,
            },
            &fixture.caller,
        )
        .unwrap_err();
    assert_eq!(invalid.code, ErrorCode::ParamsInvalid);

    // The two address forms share a retained log. Only the log read is being
    // tested; repository registration must not wait for a domain writer.
    let held = crate::request_responsiveness_tests::HeldLane::new(
        fixture.world.database.clone(),
        crate::database::Lane::Write,
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut observations = Vec::new();
    std::thread::scope(|scope| {
        for by_id in [true, false] {
            let sender = sender.clone();
            let deployments = fixture.deployments.clone();
            let caller = fixture.caller.clone();
            let params = DeploymentLogsParams {
                deployment_id: by_id.then(|| first.deployment_id.clone()),
                path: (!by_id).then(|| fixture.world.worktree.to_string_lossy().into_owned()),
                name: (!by_id).then(|| "web".into()),
                component: "removed".into(),
                generation: Some(7),
                attempt: None,
                tail_lines: 10,
            };
            scope.spawn(move || {
                let began = Instant::now();
                let result = deployments.logs(params, &caller);
                sender.send((by_id, began.elapsed(), result)).unwrap();
            });
        }
        for _ in 0..2 {
            if let Ok(result) = receiver.recv_timeout(Duration::from_secs(1)) {
                observations.push(result);
            }
        }
        // Always release the fixture's writers and join accepted reads before
        // asserting, including the intentionally failing baseline.
        drop(held);
    });
    observations.extend(receiver.try_iter());
    let mut failures = Vec::new();
    for (by_id, elapsed, result) in observations {
        match result {
            Ok(logs) => {
                assert_eq!(logs.tail, "removed component startup");
                if elapsed >= Duration::from_secs(1) {
                    failures.push(format!(
                        "by_id={by_id} waited {} ms for a writer",
                        elapsed.as_millis()
                    ));
                }
            }
            Err(error) => failures.push(format!("by_id={by_id}: {}", error.code)),
        }
    }
    let declaration = fixture.world.worktree.join(".devcoordinator.toml");
    let original = std::fs::read(&declaration).unwrap();
    for (state, bytes) in [("malformed", Some("schema = [\n")), ("removed", None)] {
        if let Some(bytes) = bytes {
            std::fs::write(&declaration, bytes).unwrap();
        } else {
            std::fs::remove_file(&declaration).unwrap();
        }
        match fixture.deployments.logs(
            DeploymentLogsParams {
                deployment_id: Some(first.deployment_id.clone()),
                path: None,
                name: None,
                component: "removed".into(),
                generation: Some(7),
                attempt: None,
                tail_lines: 10,
            },
            &fixture.caller,
        ) {
            Ok(logs) => assert_eq!(logs.tail, "removed component startup"),
            Err(error) => failures.push(format!(
                "retained logs with {state} declaration: {}",
                error.code
            )),
        }
        assert!(
            fixture
                .deployments
                .preflight(
                    None,
                    None,
                    Some(&first.deployment_id),
                    None,
                    &fixture.caller
                )
                .is_err(),
            "preflight must still validate desired source"
        );
        assert!(
            fixture
                .deployments
                .apply(None, None, Some(&first.deployment_id), &fixture.caller)
                .is_err(),
            "apply must still validate desired source"
        );
        std::fs::write(&declaration, &original).unwrap();
    }
    assert!(
        failures.is_empty(),
        "retained log reads depend on writes or mutable source: {failures:?}"
    );
}

#[test]
fn process_unit_readiness_waits_through_activation_within_health_budget() {
    let fixture = RouteWorld::new("worktree");
    fixture.systemd.activation_polls.store(12, Ordering::SeqCst);
    let applied = fixture.apply().unwrap();
    assert_eq!(applied.state, "running");
    assert!(applied.readiness.unwrap().ready);
}

#[test]
fn mixed_candidate_rollback_uses_retained_configuration_and_restores_failed_snapshot() {
    let fixture = RouteWorld::new("checkout");
    let config_path = fixture.world.worktree.join(".devcoordinator.toml");
    let original = std::fs::read_to_string(&config_path).unwrap();
    let first = fixture.apply().unwrap();
    let id = &first.deployment_id;
    let candidate_spec = original.replace("[\"serve\"]", "[\"serve-candidate\"]");
    std::fs::write(&config_path, &candidate_spec).unwrap();
    fixture.change();
    let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
        path: fixture.world.worktree.to_string_lossy().into_owned(),
        commit: "b".repeat(40),
    };
    fixture
        .deployments
        .apply_with_candidate(None, None, Some(id), Some(&candidate), &fixture.caller)
        .unwrap();
    let before = fixture.deployments.store.get(id).unwrap().unwrap();
    std::fs::write(
        &config_path,
        original.replace("[\"serve\"]", "[\"unapplied-command\"]"),
    )
    .unwrap();
    let route_path = fixture.deployments.config.routes_path();
    let blocked_path = route_path.clone();
    *fixture.systemd.listener_hook.lock().unwrap() = Some(Box::new(move || {
        std::fs::rename(&blocked_path, blocked_path.with_extension("retained")).unwrap();
        std::fs::create_dir(&blocked_path).unwrap();
    }));
    assert!(
        fixture
            .deployments
            .rollback(None, None, Some(id), &fixture.caller)
            .is_err()
    );
    let restored = fixture.deployments.store.get(id).unwrap().unwrap();
    assert_eq!(restored.spec_json, before.spec_json);
    assert_eq!(restored.spec_fingerprint, before.spec_fingerprint);
    assert_eq!(restored.current_generation, before.current_generation);
    assert_eq!(restored.previous_generation, before.previous_generation);
    std::fs::remove_dir(&route_path).unwrap();
    std::fs::rename(route_path.with_extension("retained"), &route_path).unwrap();
    std::fs::write(&config_path, "schema = [\n").unwrap();
    let to_ordinary = fixture
        .deployments
        .rollback(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, to_ordinary.current_generation.unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        stored_deployment_specification(&fixture.deployments.store.get(id).unwrap().unwrap())
            .unwrap()
            .components[0]
            .command,
        ["serve"]
    );
    let to_candidate = fixture
        .deployments
        .rollback(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, to_candidate.current_generation.unwrap())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        stored_deployment_specification(&fixture.deployments.store.get(id).unwrap().unwrap())
            .unwrap()
            .components[0]
            .command,
        ["serve-candidate"]
    );
    assert_eq!(to_candidate.route_port, first.route_port);
    fixture
        .deployments
        .control("restart", None, None, Some(id), None, &fixture.caller)
        .unwrap();
    assert_eq!(
        stored_deployment_specification(&fixture.deployments.store.get(id).unwrap().unwrap())
            .unwrap()
            .components[0]
            .command,
        ["serve-candidate"]
    );
}

#[test]
fn explicit_candidate_origin_changes_require_a_new_generation_even_for_identical_content() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let id = &first.deployment_id;
    let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
        path: fixture.world.worktree.to_string_lossy().into_owned(),
        commit: "a".repeat(40),
    };
    let bind = || {
        fixture
            .deployments
            .apply_with_candidate(None, None, Some(id), Some(&candidate), &fixture.caller)
            .unwrap()
    };
    assert_eq!(bind().current_generation, Some(2));
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, 1)
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, 2)
            .unwrap()
            .is_some()
    );
    assert_eq!(bind().current_generation, Some(2));
    let ordinary = fixture
        .deployments
        .apply(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert_eq!(ordinary.current_generation, Some(3));
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, 3)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .deployments
            .apply(None, None, Some(id), &fixture.caller)
            .unwrap()
            .current_generation,
        Some(3)
    );
    assert_eq!(bind().current_generation, Some(4));
    let before = fixture.deployments.store.generation_origin(id, 4).unwrap();
    let rolled = fixture
        .deployments
        .rollback(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert_eq!(rolled.current_generation, Some(5));
    assert!(
        fixture
            .deployments
            .store
            .generation_origin(id, 5)
            .unwrap()
            .is_none()
    );
    let rolled = fixture
        .deployments
        .rollback(None, None, Some(id), &fixture.caller)
        .unwrap();
    assert_eq!(rolled.current_generation, Some(6));
    assert_eq!(
        fixture.deployments.store.generation_origin(id, 6).unwrap(),
        before
    );
    assert!(fixture.status(id).readiness.unwrap().ready);
}

#[test]
fn explicit_candidate_keeps_deployment_identity_and_previous_generation() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    fixture.change();
    let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
        path: fixture.world.worktree.to_string_lossy().into_owned(),
        commit: "b".repeat(40),
    };
    let applied = fixture
        .deployments
        .apply_request(
            &devcoordinator2_api::params::DeploymentApply {
                deployment_id: Some(first.deployment_id.clone()),
                candidate: Some(candidate.clone()),
                ..Default::default()
            },
            &fixture.caller,
        )
        .unwrap();
    assert_eq!(applied.deployment_id, first.deployment_id);
    assert_eq!(applied.current_generation, Some(2));
    assert_eq!(applied.previous_generation, Some(1));
    assert_eq!(applied.domain, first.domain);
    let repeated = fixture
        .deployments
        .apply_request(
            &devcoordinator2_api::params::DeploymentApply {
                deployment_id: Some(first.deployment_id.clone()),
                candidate: Some(candidate),
                ..Default::default()
            },
            &fixture.caller,
        )
        .unwrap();
    assert_eq!(repeated.current_generation, Some(2));
}

#[test]
fn invalid_candidate_source_never_changes_the_running_deployment() {
    for failure in [
        "dirty",
        "commit",
        "repository",
        "undeclared-source",
        "missing-target",
        "repeat",
    ] {
        let fixture = RouteWorld::new("checkout");
        let first = fixture.apply().unwrap();
        let foreign = RouteWorld::new("checkout");
        fixture.change();
        let config_path = fixture.world.worktree.join(".devcoordinator.toml");
        let original_config = std::fs::read_to_string(&config_path).unwrap();
        if failure == "undeclared-source" {
            std::fs::write(
                &config_path,
                original_config.replace("source=\"checkout\"", "source=\"worktree\""),
            )
            .unwrap();
        }
        if failure == "dirty" {
            fixture.git.snapshot.lock().unwrap().dirty = true;
        }
        let candidate = devcoordinator2_api::params::DeploymentCandidateSource {
            path: (if failure == "repository" {
                &foreign.world.worktree
            } else {
                &fixture.world.worktree
            })
            .to_string_lossy()
            .into_owned(),
            commit: if failure == "commit" {
                "c".repeat(40)
            } else {
                "b".repeat(40)
            },
        };
        let before = fixture.git.actions.lock().unwrap().len();
        let target = if failure == "missing-target" {
            None
        } else {
            Some(first.deployment_id.as_str())
        };
        assert!(
            fixture
                .deployments
                .apply_request(
                    &devcoordinator2_api::params::DeploymentApply {
                        deployment_id: target.map(str::to_owned),
                        candidate: Some(candidate),
                        repeat_finite_after: (failure == "repeat").then_some(1),
                        ..Default::default()
                    },
                    &fixture.caller,
                )
                .is_err(),
            "{failure}"
        );
        assert_eq!(
            fixture.git.actions.lock().unwrap().len(),
            before,
            "{failure}"
        );
        // Restore the declaration before the public status read; an invalid
        // declaration is correctly rejected by that read as well as by apply.
        if failure == "undeclared-source" {
            std::fs::write(&config_path, original_config).unwrap();
        }
        let after = fixture.status(&first.deployment_id);
        assert_eq!(
            after.current_generation, first.current_generation,
            "{failure}"
        );
        assert_eq!(after.route_port, first.route_port, "{failure}");
    }
}

struct RouteWorld {
    world: World,
    deployments: Deployments,
    git: Arc<CheckoutGit>,
    systemd: Arc<MutationSystemd>,
    caller: Caller,
}

impl RouteWorld {
    fn reopen(&self) -> Deployments {
        let config = (*self.world.deployments.config).clone();
        Deployments::with_runtime_adapters(
            config.clone(),
            self.world.database.clone(),
            Registry::new(self.world.database.clone()),
            Arc::new(MutationDocker::new()),
            self.systemd.clone(),
            Arc::new(ReadyNetwork),
            self.git.clone(),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(HostClock),
        )
    }

    fn new(source: &str) -> Self {
        let mut world = World::new();
        // Let the real registry assign identities; World::new seeds synthetic IDs.
        world.database =
            Database::open(world._temporary.path().join("route-authority.sqlite3")).unwrap();
        // Replace only the deliberately invalid Git marker of this disposable fixture.
        std::fs::remove_file(world.worktree.join(".git")).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&world.worktree)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            world.worktree.join(".devcoordinator.toml"),
            format!(
                r#"
schema=2
[deployment.web]
source="{source}"
domain="app"
components=["api"]
[deployment.web.component.api]
type="process"
command=["serve"]
port=true
route=true
"#
            ),
        )
        .unwrap();
        let config = (*world.deployments.config).clone();
        let git = Arc::new(CheckoutGit::new());
        let systemd = Arc::new(MutationSystemd::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            world.database.clone(),
            Registry::new(world.database.clone()),
            Arc::new(MutationDocker::new()),
            systemd.clone(),
            Arc::new(ReadyNetwork),
            git.clone(),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(HostClock),
        );
        let mut caller = World::caller();
        caller.uid = rustix::process::getuid().as_raw();
        caller.gid = rustix::process::getgid().as_raw();
        assert_ne!(caller.uid, 0);
        Self {
            world,
            deployments,
            git,
            systemd,
            caller,
        }
    }

    fn apply(&self) -> Result<DeploymentStatus, ProtocolError> {
        self.deployments.apply(
            Some(self.world.worktree.to_str().unwrap()),
            Some("web"),
            None,
            &self.caller,
        )
    }

    fn change(&self) {
        self.git.snapshot.lock().unwrap().commit = Some("b".repeat(40));
    }

    fn route(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.deployments.config.routes_path()).unwrap())
            .unwrap()
    }

    fn status(&self, id: &str) -> DeploymentStatus {
        self.deployments
            .status(None, None, Some(id), &self.caller)
            .unwrap()
    }
}

#[test]
fn failed_worktree_candidate_is_not_reused_when_its_unit_is_still_running() {
    let fixture = RouteWorld::new("worktree");
    let first = fixture.apply().unwrap();
    let initial_route = fixture.route();
    let occupied = first.components[0]
        .binding
        .identity
        .as_ref()
        .unwrap()
        .replace("-g1.service", "-g2.service");
    assert!(occupied.ends_with("-g2.service"));
    fixture
        .systemd
        .states
        .lock()
        .unwrap()
        .insert(occupied.clone(), "running".into());
    fixture
        .systemd
        .reject_running_starts
        .store(true, Ordering::SeqCst);
    fixture.change();
    let failure = fixture.apply().unwrap_err();
    assert!(
        failure
            .message
            .contains("cannot recreate a non-terminal unit")
    );
    let failed = fixture.status(&first.deployment_id);
    assert_eq!(failed.current_generation, Some(1));
    assert_eq!(failed.components[0].state, "running");
    assert!(!failed.readiness.unwrap().ready);
    assert_eq!(fixture.route()["routes"], initial_route["routes"]);
    let retained = fixture
        .deployments
        .store
        .generations(&first.deployment_id)
        .unwrap();
    assert!(
        retained
            .iter()
            .any(|row| row.number == 2 && row.state == "failed")
    );
    let recovered = fixture.apply().unwrap();
    assert_eq!(recovered.current_generation, Some(3));
    assert_eq!(recovered.route_port, first.route_port);
    assert!(recovered.readiness.unwrap().ready);
    assert_eq!(
        fixture.route()["routes"][0]["lease_id"],
        initial_route["routes"][0]["lease_id"]
    );
    assert_eq!(
        fixture
            .systemd
            .states
            .lock()
            .unwrap()
            .get(&occupied)
            .map(String::as_str),
        Some("running")
    );
}

#[test]
fn publication_during_process_transition_does_not_abort_candidate() {
    for source in ["worktree", "checkout"] {
        let fixture = RouteWorld::new(source);
        let first = fixture.apply().unwrap();
        let lease = fixture.route()["routes"][0]["lease_id"].clone();
        fixture.change();
        let publisher = fixture.deployments.routes.clone();
        let database = fixture.world.database.clone();
        let id = first.deployment_id.clone();
        *fixture.systemd.listener_hook.lock().unwrap() = Some(Box::new(move || {
            // A background publisher observes healthy g2 beside the still-selected g1 route.
            let document = publisher.publish_current().unwrap();
            assert!(
                document
                    .routes
                    .iter()
                    .all(|route| route.deployment_id != id)
            );
            let store = DeploymentStore::new(database);
            assert_eq!(store.get(&id).unwrap().unwrap().state, "applying");
        }));
        let second = fixture.apply().unwrap();
        assert_eq!(second.current_generation, Some(2));
        assert_eq!(second.components[0].generation, Some(2));
        assert_eq!(second.route_port, first.route_port);
        assert!(second.readiness.unwrap().ready);
        let route = fixture.route();
        assert_eq!(route["routes"][0]["lease_id"], lease);
        assert_eq!(route["routes"][0]["generation"], 2);
    }
}

#[test]
fn publication_failure_restores_previous_runtime_and_checkout_can_retry() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let original = fixture.route();
    let route_path = fixture.deployments.config.routes_path();
    fixture.change();
    let blocked_path = route_path.clone();
    *fixture.systemd.listener_hook.lock().unwrap() = Some(Box::new(move || {
        std::fs::rename(&blocked_path, blocked_path.with_extension("retained")).unwrap();
        std::fs::create_dir(&blocked_path).unwrap();
    }));
    let error = fixture.apply().unwrap_err();
    assert_eq!(error.code, ErrorCode::DeploymentApplyFailed);
    let failed = fixture.status(&first.deployment_id);
    assert_eq!(failed.current_generation, Some(1));
    assert_eq!(failed.components[0].generation, Some(1));
    assert_eq!(failed.components[0].state, "running");
    assert!(!failed.readiness.unwrap().ready);
    // Recovery of the injected private output fault; no live route file is involved.
    std::fs::remove_dir(&route_path).unwrap();
    std::fs::rename(route_path.with_extension("retained"), &route_path).unwrap();
    fixture.deployments.reconcile_routes().unwrap();
    assert_eq!(fixture.route()["routes"], original["routes"]);
    let recovered = fixture.apply().unwrap();
    assert_eq!(recovered.route_port, first.route_port);
    assert!(recovered.current_generation.unwrap() > 1);
    assert!(recovered.readiness.unwrap().ready);
    assert_eq!(
        fixture.route()["routes"][0]["lease_id"],
        original["routes"][0]["lease_id"]
    );
}

#[test]
fn retained_uncommitted_checkout_generation_is_not_reused() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let row = fixture
        .deployments
        .store
        .get(&first.deployment_id)
        .unwrap()
        .unwrap();
    let orphan = fixture
        .deployments
        .files
        .generation_path(&first.deployment_id, 2)
        .unwrap();
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("retained-evidence"), "failed publication").unwrap();
    fixture
        .deployments
        .store
        .add_generation(
            &first.deployment_id,
            2,
            Some(&"b".repeat(40)),
            false,
            &orphan,
            &row.spec_fingerprint,
        )
        .unwrap();
    fixture.change();
    let recovered = fixture.apply().unwrap();
    assert_eq!(recovered.current_generation, Some(3));
    assert!(recovered.readiness.unwrap().ready);
}

#[test]
fn healthy_process_without_its_route_is_not_ready() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let id = first.deployment_id.clone();
    fixture
        .world
        .database
        .transaction(move |connection| crate::ports::withdraw_conflict(connection, &id, "api"))
        .unwrap();
    let status = fixture.status(&first.deployment_id);
    assert_eq!(status.components[0].state, "running");
    assert!(status.route_port.is_none());
    let readiness = status.readiness.unwrap();
    assert!(!readiness.ready);
    let repair = readiness
        .next_actions
        .iter()
        .find(|action| action.id == "repair-route")
        .expect("current-source route withdrawal must be actionable");
    assert_eq!(repair.operation.as_deref(), Some("deployment.apply"));
    assert_eq!(repair.target.as_deref(), Some(first.deployment_id.as_str()));
    let recovered = fixture.apply().unwrap();
    assert!(recovered.readiness.unwrap().ready);
    assert_eq!(recovered.route_port, first.route_port);
}

#[test]
fn reconcile_recovers_missing_saved_process_on_original_route_lease() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let starts_before_reconcile = fixture
        .systemd
        .actions
        .lock()
        .unwrap()
        .iter()
        .filter(|action| action.starts_with("start:"))
        .count();
    fixture.deployments.reconcile_routes().unwrap();
    assert_eq!(
        fixture
            .systemd
            .actions
            .lock()
            .unwrap()
            .iter()
            .filter(|action| action.starts_with("start:"))
            .count(),
        starts_before_reconcile,
        "healthy same-generation unit was restarted",
    );
    let initial = fixture.route();
    let unit = first.components[0].binding.identity.clone().unwrap();
    fixture.systemd.states.lock().unwrap().remove(&unit);
    fixture
        .world
        .database
        .transaction({
            let id = first.deployment_id.clone();
            move |connection| crate::ports::withdraw_conflict(connection, &id, "api")
        })
        .unwrap();
    let declaration = fixture.world.worktree.join(".devcoordinator.toml");
    let original = std::fs::read(&declaration).unwrap();
    std::fs::write(&declaration, "schema = [\n").unwrap();
    let reconciled = fixture.deployments.reconcile_routes();
    let restarted = fixture
        .systemd
        .states
        .lock()
        .unwrap()
        .get(&unit)
        .is_some_and(|state| state == "running");
    std::fs::write(&declaration, original).unwrap();
    assert!(
        reconciled.is_ok() && restarted,
        "route recovery must use the recorded declaration: {reconciled:?}"
    );
    let recovered = fixture.status(&first.deployment_id);
    assert_eq!(recovered.state, "running");
    assert_eq!(recovered.current_generation, first.current_generation);
    assert_eq!(recovered.route_port, first.route_port);
    assert_eq!(
        recovered.components[0].binding.identity.as_deref(),
        Some(unit.as_str())
    );
    assert_eq!(
        fixture.route()["routes"][0]["lease_id"],
        initial["routes"][0]["lease_id"]
    );
    assert!(
        fixture
            .systemd
            .actions
            .lock()
            .unwrap()
            .iter()
            .any(|action| action == &format!("start:{unit}"))
    );

    // Hold the real reconciliation path inside live listener validation. A
    // healthy status can already be visible here; the next mutation must hand
    // off asynchronously rather than report a foreground conflict.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *fixture.systemd.listener_hook.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        let service = fixture.deployments.clone();
        let reconciliation = tokio::task::spawn_blocking(move || service.reconcile_routes());
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            fixture
                .status(&first.deployment_id)
                .readiness
                .unwrap()
                .ready
        );
        let admission = fixture
            .deployments
            .prepare_mutation_admission(None, None, Some(&first.deployment_id), &fixture.caller)
            .unwrap()
            .unwrap();
        let crate::daemon::PreparedAdmission::Waiting(wait) = admission else {
            panic!("maintenance was not held by the reconciliation hook");
        };
        let admitted = tokio::spawn(wait);
        release_tx.send(()).unwrap();
        reconciliation.await.unwrap().unwrap();
        let hold = admitted.await.unwrap().unwrap();
        assert!(
            fixture
                .deployments
                .acquire_mutation(&first.deployment_id, MutationOwner::Maintenance,)
                .is_err(),
            "maintenance reacquired ahead of the admitted foreground mutation"
        );
        let restarted = fixture
            .deployments
            .control(
                "restart",
                None,
                None,
                Some(&first.deployment_id),
                Some("api"),
                &fixture.caller,
            )
            .unwrap();
        assert_eq!(restarted.current_generation, first.current_generation);
        assert_eq!(restarted.route_port, first.route_port);
        drop(hold);
        assert!(fixture.deployments.busy.entries.lock().unwrap().is_empty());

        // A true foreground overlap remains Busy, and even a rejected execution
        // releases the intent that kept maintenance from barging in.
        let foreground = fixture
            .deployments
            .acquire_busy(&first.deployment_id)
            .unwrap();
        let crate::daemon::PreparedAdmission::Ready(hold) = fixture
            .deployments
            .foreground_admission(&first.deployment_id, Duration::from_secs(15))
        else {
            panic!("foreground conflict was turned into a maintenance wait");
        };
        assert_eq!(
            fixture
                .deployments
                .control(
                    "restart",
                    None,
                    None,
                    Some(&first.deployment_id),
                    Some("api"),
                    &fixture.caller,
                )
                .unwrap_err()
                .code,
            ErrorCode::Busy
        );
        drop(hold);
        drop(foreground);
        assert!(fixture.deployments.busy.entries.lock().unwrap().is_empty());

        let maintenance = crate::database::maintenance_context(|| {
            fixture.deployments.acquire_busy(&first.deployment_id)
        })
        .unwrap();
        let crate::daemon::PreparedAdmission::Waiting(wait) = fixture
            .deployments
            .foreground_admission(&first.deployment_id, Duration::ZERO)
        else {
            panic!("missing maintenance wait");
        };
        let error = match wait.await {
            Ok(_) => panic!("held maintenance was admitted"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::Busy);
        assert!(error.message.contains("mutation was not started"));
        let crate::daemon::PreparedAdmission::Waiting(cancelled) = fixture
            .deployments
            .foreground_admission(&first.deployment_id, Duration::from_secs(15))
        else {
            panic!("missing cancellable maintenance wait");
        };
        let cancelled = tokio::spawn(cancelled);
        cancelled.abort();
        let Err(cancelled) = cancelled.await else {
            panic!("cancelled admission completed");
        };
        assert!(cancelled.is_cancelled());
        assert_eq!(
            fixture.deployments.busy.entries.lock().unwrap()[&first.deployment_id].pending,
            0
        );
        drop(maintenance);
        assert!(fixture.deployments.busy.entries.lock().unwrap().is_empty());
    });
}

#[test]
fn restoring_only_a_route_keeps_the_original_process_log_identity() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let read = || {
        fixture
            .deployments
            .logs(
                DeploymentLogsParams {
                    deployment_id: Some(first.deployment_id.clone()),
                    path: None,
                    name: None,
                    component: "api".into(),
                    generation: Some(1),
                    attempt: None,
                    tail_lines: 10,
                },
                &fixture.caller,
            )
            .unwrap()
    };
    let before = read();
    fixture
        .world
        .database
        .transaction({
            let id = first.deployment_id.clone();
            move |connection| crate::ports::withdraw_conflict(connection, &id, "api")
        })
        .unwrap();
    fixture.deployments.reconcile_routes().unwrap();
    let after = read();
    assert_eq!(after.tail, before.tail);
    assert_eq!(
        after.process_log.unwrap().attempt,
        before.process_log.unwrap().attempt,
        "restoring a route without starting the active unit created a fictional startup attempt"
    );
    assert!(
        fixture
            .status(&first.deployment_id)
            .readiness
            .unwrap()
            .ready
    );
}

#[test]
fn reconcile_does_not_restart_stopped_or_repair_a_mismatched_route_lease() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    fixture
        .deployments
        .control(
            "stop",
            None,
            None,
            Some(&first.deployment_id),
            None,
            &fixture.caller,
        )
        .unwrap();
    let starts = fixture
        .systemd
        .actions
        .lock()
        .unwrap()
        .iter()
        .filter(|action| action.starts_with("start:"))
        .count();
    fixture.deployments.reconcile_routes().unwrap();
    assert_eq!(
        fixture
            .systemd
            .actions
            .lock()
            .unwrap()
            .iter()
            .filter(|action| action.starts_with("start:"))
            .count(),
        starts
    );

    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let mismatch_starts = fixture
        .systemd
        .actions
        .lock()
        .unwrap()
        .iter()
        .filter(|action| action.starts_with("start:"))
        .count();
    fixture
        .world
        .database
        .transaction({
            let id = first.deployment_id.clone();
            move |connection| {
                connection.execute(
                    "UPDATE domain_routes SET lease_id='foreign-lease' WHERE deployment_id=$1",
                    [&id],
                )?;
                Ok(())
            }
        })
        .unwrap();
    fixture
        .systemd
        .states
        .lock()
        .unwrap()
        .remove(&first.components[0].binding.identity.clone().unwrap());
    fixture.deployments.reconcile_routes().unwrap();
    assert_eq!(
        fixture
            .systemd
            .actions
            .lock()
            .unwrap()
            .iter()
            .filter(|action| action.starts_with("start:"))
            .count(),
        mismatch_starts
    );
}

#[test]
fn first_publication_failure_can_retry_without_a_previous_generation() {
    let fixture = RouteWorld::new("checkout");
    let route_path = fixture.deployments.config.routes_path();
    std::fs::create_dir_all(&route_path).unwrap();
    let error = fixture.apply().unwrap_err();
    assert_eq!(error.code, ErrorCode::DeploymentApplyFailed);
    let rows = fixture.deployments.store.list(None).unwrap();
    let row = rows.first().unwrap();
    let failed = fixture.status(&row.deployment_id);
    assert!(failed.current_generation.is_none());
    assert!(failed.route_port.is_none());
    assert!(!failed.readiness.unwrap().ready);
    std::fs::remove_dir(&route_path).unwrap();
    assert!(fixture.apply().unwrap().readiness.unwrap().ready);
}

#[test]
fn unrecorded_checkout_path_is_preserved_and_generation_mismatch_is_not_ready() {
    let fixture = RouteWorld::new("checkout");
    let first = fixture.apply().unwrap();
    let orphan = fixture
        .deployments
        .files
        .generation_path(&first.deployment_id, 2)
        .unwrap();
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("retained-evidence"), "interrupted preparation").unwrap();
    fixture.change();
    let recovered = fixture.apply().unwrap();
    assert_eq!(recovered.current_generation, Some(3));
    assert!(orphan.join("retained-evidence").is_file());
    fixture
        .deployments
        .store
        .set_component_runtime(
            &first.deployment_id,
            "api",
            ComponentRuntimePatch {
                generation: Some(Some(4)),
                ..Default::default()
            },
        )
        .unwrap();
    let status = fixture.status(&first.deployment_id);
    assert!(!status.readiness.unwrap().ready);
    let published = fixture.deployments.routes.publish_current().unwrap();
    assert!(published.routes.is_empty());
}
