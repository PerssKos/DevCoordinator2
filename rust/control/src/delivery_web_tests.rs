use super::*;

#[test]
fn web_candidate_origin_is_checked_before_qualification_and_never_substituted() {
    for source in ["checkout", "worktree"] {
        let world = World::new(Kind::WebDeployment, Some(proof(Kind::WebDeployment)));
        let path = &world.fixture.repository;
        std::fs::write(path.join(".devcoordinator.toml"),
        format!("schema=2\n[deployment.web]\nsource=\"{source}\"\ncomponents=[\"web\"]\n[deployment.web.component.web]\ntype=\"process\"\ncommand=[\"serve\"]\nport=true\nroute=true\n")).unwrap();
        std::fs::write(path.join(".gitignore"), ".devcoordinator/\n").unwrap();
        let valid_spec = std::fs::read_to_string(path.join(".devcoordinator.toml")).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", ".devcoordinator.toml", ".gitignore"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "Fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(path)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let resolved = crate::repository::resolve_worktree(path, Some((uid, gid))).unwrap();
        let repository_id = crate::ids::repository_id(&resolved.repository_root).unwrap();
        let origin = crate::deployment_state::GenerationSourceOrigin {
            worktree_id: crate::ids::worktree_id(&resolved.worktree_root).unwrap(),
            path: resolved.worktree_root,
        };
        let snapshot = crate::deployment_git::DeploymentGit::snapshot(
            &crate::deployment_git::GitCli,
            path,
            uid,
            gid,
        )
        .unwrap();
        let spec = crate::repository_config::load_deployment_spec(path, "web")
            .unwrap()
            .canonical(source);
        let fingerprint = DeploymentStore::fingerprint(
            &json!({"spec":spec,"commit":snapshot.commit,"dirty":snapshot.dirty,"source_digest":snapshot.source_digest}),
        );
        let mut params = world.params.clone();
        params.source_sha256 = snapshot.source_digest;
        let mut verification = proof(Kind::WebDeployment);
        verification.source_sha256 = params.source_sha256.clone();
        let actual_id = repository_id.clone();
        let root = path.to_string_lossy().into_owned();
        let retained = world
            .service
            .deployment_files
            .generation_path("d1111111111111111", 1)
            .unwrap();
        std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
        crate::deployment_git::DeploymentGit::add_detached_worktree(
            &crate::deployment_git::GitCli,
            path,
            &retained,
            snapshot.commit.as_deref().unwrap(),
            uid,
            gid,
        )
        .unwrap();
        let retained_path = retained.to_string_lossy().into_owned();
        world.fixture.database.transaction(move |db| {
        db.execute("UPDATE repositories SET root_path='/fixture/unused-legacy' WHERE repository_id='project-alpha'", [])?;
        db.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES($1,$2,'Candidate','t',$3,'t')", crate::database::params![actual_id,root,uid])?;
        db.execute("UPDATE deployments SET repository_id=$1,source=$5,created_by_uid=$2,spec_json=$3,spec_fingerprint=$4 WHERE deployment_id='d1111111111111111'", crate::database::params![actual_id,uid,spec.to_string(),fingerprint,source])?;
        db.execute("UPDATE generations SET commit_hash=$1,dirty=$2,fingerprint=$3,path=$4 WHERE deployment_id='d1111111111111111'", crate::database::params![snapshot.commit,i64::from(snapshot.dirty),fingerprint,retained_path])?;
        db.execute("INSERT INTO generation_source_origins(deployment_id,number,worktree_id,path) VALUES('d1111111111111111',1,$1,$2)", crate::database::params![origin.worktree_id,origin.path.to_string_lossy().into_owned()])?;
        Ok(())
    }).unwrap();
        world
            .service
            .validate_web_deployment(&params, &verification, &repository_id)
            .unwrap();
        let mutable_path = path.to_string_lossy().into_owned();
        world
            .fixture
            .database
            .transaction(move |db| {
                db.execute(
                    "UPDATE generations SET path=$1 WHERE deployment_id='d1111111111111111'",
                    [mutable_path],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(
            world
                .service
                .validate_web_deployment(&params, &verification, &repository_id)
                .is_err(),
            "mutable source was accepted as an exact frozen candidate"
        );
        let retained_path = retained.to_string_lossy().into_owned();
        world
            .fixture
            .database
            .transaction(move |db| {
                db.execute(
                    "UPDATE generations SET path=$1 WHERE deployment_id='d1111111111111111'",
                    [retained_path],
                )?;
                Ok(())
            })
            .unwrap();
        std::fs::write(path.join(".devcoordinator.toml"), "schema=2\n").unwrap();
        assert!(
            world
                .service
                .validate_web_deployment(&params, &verification, &repository_id)
                .is_err()
        );
        std::fs::write(path.join(".devcoordinator.toml"), &valid_spec).unwrap();
        world
            .service
            .validate_web_deployment(&params, &verification, &repository_id)
            .unwrap();
        let foreign = world.fixture._temporary.path().join("foreign-source");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join(".devcoordinator.toml"), valid_spec).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", ".devcoordinator.toml"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "Foreign fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&foreign)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let foreign_origin = crate::ids::worktree_id(&foreign).unwrap();
        let foreign_path = foreign.to_string_lossy().into_owned();
        world.fixture.database.transaction(move |db| {
        db.execute("UPDATE generation_source_origins SET path=$1,worktree_id=$2 WHERE deployment_id='d1111111111111111'", crate::database::params![foreign_path,foreign_origin])?;
        Ok(())
    }).unwrap();
        let refused = world
            .service
            .validate_web_deployment(&params, &verification, &repository_id)
            .unwrap_err();
        assert_eq!(
            refused.code,
            devcoordinator2_api::ErrorCode::DeploymentApplyFailed
        );
        assert!(refused.message.contains("recorded candidate source origin"));
    }
}

#[test]
fn native_console_receipt_binds_running_source_and_persists_without_a_deployment() {
    let world = World::new(Kind::NativeConsole, Some(proof(Kind::NativeConsole)));
    let receipt = world
        .service
        .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
        .unwrap();
    assert!(receipt.qualified);
    assert_eq!(receipt.delivered_at_ms, Some(FINISHED - 100));
    assert_eq!(
        receipt.access.as_deref(),
        Some("https://console.example.test/")
    );
    let stored = world
        .service
        .receipt(Reference {
            reference: receipt.receipt_id.clone(),
        })
        .unwrap();
    assert_eq!(stored, receipt);
    let deployments = world
        .fixture
        .database
        .call(|c| {
            Ok(c.query_row("SELECT count(*) FROM deployments", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .unwrap();
    assert_eq!(deployments, 0);
}

#[test]
fn native_console_refuses_foreign_incomplete_or_stale_observations() {
    let mutations: &[fn(&mut Verification)] = &[
        |p| p.native_console = None,
        |p| p.native_console.as_mut().unwrap().daemon_source_commit = "d".repeat(40),
        |p| p.native_console.as_mut().unwrap().assets_sha256 = "f".repeat(64),
        |p| p.native_console.as_mut().unwrap().http_status = 401,
        |p| p.native_console.as_mut().unwrap().content_type = "application/json".into(),
        |p| p.access = "https://other.example.test/".into(),
        |p| p.access = "https://console.example.test/auth/login".into(),
        |p| p.access = "https://console.example.test/?secret=private".into(),
        |p| p.access = "https://console.example.test/#/bugs".into(),
        |p| p.deployment = proof(Kind::WebDeployment).deployment,
        |p| p.observed_sha256 = "f".repeat(64),
        |p| p.observation = VerificationObservation::DownloadMatched,
    ];
    for mutate in mutations {
        let mut verification = proof(Kind::NativeConsole);
        mutate(&mut verification);
        let world = World::new(Kind::NativeConsole, Some(verification));
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err()
        );
    }
}

#[test]
fn native_console_refuses_changed_checkout_or_unbound_service() {
    for scenario in 0..4 {
        let mut world = World::new(Kind::NativeConsole, Some(proof(Kind::NativeConsole)));
        match scenario {
            0 => std::fs::write(
                world.fixture.repository.join("console/index.html"),
                b"changed index",
            )
            .unwrap(),
            1 => std::fs::write(
                world.fixture.repository.join("console/app.js"),
                b"changed app",
            )
            .unwrap(),
            2 => world.service.native_console = None,
            _ => {
                world.service.native_console = Some((
                    world.fixture._temporary.path().to_path_buf(),
                    "c".repeat(40),
                ))
            }
        }
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err()
        );
    }
}

#[test]
fn other_delivery_kinds_cannot_borrow_native_console_metadata() {
    let mut verification = proof(Kind::Artifact);
    verification.native_console = proof(Kind::NativeConsole).native_console;
    let world = World::new(Kind::Artifact, Some(verification));
    assert!(
        world
            .service
            .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
            .is_err()
    );
}

#[test]
fn native_console_asset_fingerprint_matches_only_published_static_types() {
    let world = World::new(Kind::NativeConsole, Some(proof(Kind::NativeConsole)));
    let before = console_assets_digest(&world.fixture.repository).unwrap();
    std::fs::write(
        world.fixture.repository.join("console/README.md"),
        "source documentation",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .current_dir(&world.fixture.repository)
            .args(["add", "console/README.md"])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        console_assets_digest(&world.fixture.repository).unwrap(),
        before
    );
    std::fs::write(
        world.fixture.repository.join("console/app.js"),
        "application code",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .current_dir(&world.fixture.repository)
            .args(["add", "console/app.js"])
            .status()
            .unwrap()
            .success()
    );
    assert_ne!(
        console_assets_digest(&world.fixture.repository).unwrap(),
        before
    );
    let before_renderer = console_assets_digest(&world.fixture.repository).unwrap();
    for name in ["font.ttf", "font.pfb", "characters.bcmap", "decoder.wasm"] {
        let relative = format!("console/{name}");
        std::fs::write(
            world.fixture.repository.join(&relative),
            b"renderer fixture",
        )
        .unwrap();
        assert!(
            std::process::Command::new("git")
                .current_dir(&world.fixture.repository)
                .args(["add", &relative])
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(
        world.fixture.repository.join("console/worker.mjs"),
        vec![b' '; 1_300_000],
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .current_dir(&world.fixture.repository)
            .args(["add", "console/worker.mjs"])
            .status()
            .unwrap()
            .success()
    );
    assert_ne!(
        console_assets_digest(&world.fixture.repository).unwrap(),
        before_renderer
    );
    std::fs::write(
        world.fixture.repository.join("console/worker.mjs"),
        vec![b' '; 2_097_153],
    )
    .unwrap();
    assert!(console_assets_digest(&world.fixture.repository).is_err());
}

#[test]
fn web_delivery_binds_current_generation_source_and_owned_route() {
    let mut verification = proof(Kind::WebDeployment);
    verification.access = "https://preview.example.test/airfoils/ag24".into();
    let world = World::new(Kind::WebDeployment, Some(verification.clone()));
    world.fixture.database.call(|connection| {
        connection.execute("INSERT INTO domain_routes VALUES('preview','d1111111111111111','web',24002,1,'fixture','lfixture')", [])?;
        Ok(())
    }).unwrap();
    let receipt = world
        .service
        .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
        .unwrap();
    assert!(receipt.qualified);
    assert_eq!(receipt.kind, Kind::WebDeployment);
    assert_eq!(
        receipt.access.as_deref(),
        Some(verification.access.as_str())
    );
    assert_eq!(receipt.delivered_at_ms, Some(verification.checked_at_ms));
    assert!(receipt.verification_sha256.is_some());
}

#[test]
fn web_delivery_refuses_unowned_or_incomplete_observations() {
    for mutate in [
        |proof: &mut Verification| proof.deployment = None,
        |proof: &mut Verification| proof.deployment.as_mut().unwrap().generation_number = 2,
        |proof: &mut Verification| proof.deployment.as_mut().unwrap().http_status = 404,
        |proof: &mut Verification| {
            proof.deployment.as_mut().unwrap().content_type = "application/json".into()
        },
        |proof: &mut Verification| {
            proof.access = "https://foreign.example.test/airfoils/ag24".into()
        },
        |proof: &mut Verification| proof.access = "http://127.0.0.1:24003/airfoils/ag24".into(),
        |proof: &mut Verification| {
            proof.access = "http://secret@127.0.0.1:24002/airfoils/ag24".into()
        },
        |proof: &mut Verification| {
            proof.access = "http://127.0.0.1:24002/airfoils/ag24?token=private".into()
        },
        |proof: &mut Verification| proof.observation = VerificationObservation::DownloadMatched,
    ] {
        let mut verification = proof(Kind::WebDeployment);
        mutate(&mut verification);
        let world = World::new(Kind::WebDeployment, Some(verification));
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err()
        );
    }
}

#[test]
fn web_delivery_refuses_changed_source_configuration_and_generation_state() {
    for statement in [
        "UPDATE generations SET fingerprint='changed'",
        "UPDATE generations SET dirty=1",
        "UPDATE generations SET commit_hash='different'",
        "UPDATE generations SET state='candidate'",
        "UPDATE deployments SET state='stopped'",
        "UPDATE deployments SET current_generation=2",
        "UPDATE deployments SET spec_json='{}'",
        "UPDATE generations SET created_at='2099-01-01T00:00:00Z'",
        "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('other','/other','Other','t',1,'t'); UPDATE deployments SET repository_id='other'",
    ] {
        let world = World::new(Kind::WebDeployment, Some(proof(Kind::WebDeployment)));
        world
            .fixture
            .database
            .call(move |connection| {
                connection.execute_batch(statement)?;
                Ok(())
            })
            .unwrap();
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err()
        );
    }
}

#[test]
fn non_web_kinds_cannot_borrow_web_deployment_metadata() {
    let mut verification = proof(Kind::Artifact);
    verification.deployment = proof(Kind::WebDeployment).deployment;
    let world = World::new(Kind::Artifact, Some(verification));
    assert!(
        world
            .service
            .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
            .is_err()
    );
}

#[test]
fn web_delivery_accepts_the_exact_local_route_without_changing_private_domain_access() {
    let world = World::new(Kind::WebDeployment, Some(proof(Kind::WebDeployment)));
    world.fixture.database.call(|connection| {
        connection.execute("INSERT INTO domain_routes VALUES('preview','d1111111111111111','web',24002,1,'fixture','lfixture')", [])?;
        Ok(())
    }).unwrap();
    let receipt = world
        .service
        .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
        .unwrap();
    assert!(receipt.qualified);
    assert_eq!(
        receipt.access.as_deref(),
        Some("http://127.0.0.1:24002/airfoils/ag24")
    );
    let private = world
        .fixture
        .database
        .call(|connection| {
            Ok(connection.query_row(
                "SELECT public FROM deployments WHERE deployment_id='d1111111111111111'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .unwrap();
    assert_eq!(private, 0);
}
