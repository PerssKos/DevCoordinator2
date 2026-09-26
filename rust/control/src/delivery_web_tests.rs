use super::*;

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
