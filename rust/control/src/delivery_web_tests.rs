use super::*;

#[test]
fn web_delivery_binds_current_generation_source_and_owned_route() {
    let mut verification = proof(Kind::WebDeployment);
    verification.access = "https://preview.example.test/airfoils/ag24".into();
    let world = World::new(Kind::WebDeployment, Some(verification.clone()));
    world.fixture.database.call(|connection| {
        connection.execute("INSERT INTO domain_routes VALUES('preview','d1111111111111111','web',24002,1,'fixture')", [])?;
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
        connection.execute("INSERT INTO domain_routes VALUES('preview','d1111111111111111','web',24002,1,'fixture')", [])?;
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
