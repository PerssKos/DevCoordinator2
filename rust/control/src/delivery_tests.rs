use super::*;
use crate::automation_test_support::{Fixture, START, WEEK};
use crate::repository::Registry;
use devcoordinator2_api::review::Reference;
use serde_json::json;

const RUN: &str = "t20260903T120718Z-57067c";
const FINISHED: u64 = START + WEEK + 86_400_000;

#[path = "delivery_web_tests.rs"]
mod web;

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct World {
    fixture: Fixture,
    service: DeliveryService,
    params: Deliver,
    caller: Caller,
}

impl World {
    fn new(kind: Kind, proof: Option<Verification>) -> Self {
        let fixture = Fixture::new();
        fixture.database.call(|connection| {
            connection.execute_batch("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES('release-alpha','project-alpha',1,'First CLI','preview','planned','1970-01-01T00:16:40Z','fixture','1970-01-01T00:16:40Z');")?;
            Ok(())
        }).unwrap();
        let root = fixture
            .repository
            .join(".devcoordinator/test/logs/runs")
            .join(RUN);
        let evidence = root.join("checks/build/check/evidence");
        let retained = evidence.join("retained/package");
        std::fs::create_dir_all(&retained).unwrap();
        let mut files = vec![("cli".to_owned(), b"fixture executable bytes".to_vec())];
        if let Some(proof) = proof {
            files.push(("delivery.json".into(), serde_json::to_vec(&proof).unwrap()));
        }
        let mut entries = Vec::new();
        let mut tree = Sha256::new();
        tree.update(b"devcoordinator2-retained-artifact-tree-v1\0");
        for (path, bytes) in &files {
            std::fs::write(retained.join(path), bytes).unwrap();
            let hash = digest(bytes);
            tree.update(path.as_bytes());
            tree.update(b"\0");
            tree.update(bytes.len().to_string().as_bytes());
            tree.update(b"\0");
            tree.update(hash.as_bytes());
            tree.update(b"\0");
            entries.push(json!({"path":path,"size":bytes.len(),"sha256":hash}));
        }
        let tree = tree
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let manifest = serde_json::to_vec(&json!({
            "schema":1,"kind":"devcoordinator2-retained-artifact-trees","run_id":RUN,"test":"cli-release","check":"build",
            "requested_tier":"development","readiness_eligible":false,"proof":"selected","source_sha256":"a".repeat(64),"config_sha256":"b".repeat(64),
            "artifacts":[{"name":"package","size":files.iter().map(|(_,bytes)|bytes.len()).sum::<usize>(),"files":files.len(),"sha256":tree,"entries":entries}]
        })).unwrap();
        std::fs::write(evidence.join("retained-artifacts.json"), &manifest).unwrap();
        std::fs::write(root.join("run.json"), serde_json::to_vec(&json!({"schema":2,"run_id":RUN,"test":"cli-release","started_at_epoch_ms":FINISHED-1000,"finished_at_epoch_ms":FINISHED,"status":"passed","complete":true})).unwrap()).unwrap();
        let service = DeliveryService::new(
            fixture.database.clone(),
            TestArtifactService::new(
                fixture.database.clone(),
                Registry::new(fixture.database.clone()),
            ),
            fixture.config.base_domain.clone(),
        );
        let params = Deliver {
            release_id: "release-alpha".into(),
            path: fixture.repository.to_string_lossy().into_owned(),
            run_id: RUN.into(),
            check: "build".into(),
            artifact: "package".into(),
            manifest_sha256: digest(&manifest),
            source_sha256: "a".repeat(64),
            target: "linux-cli".into(),
            kind,
            verification_file: (files.len() > 1).then(|| "delivery.json".into()),
        };
        let caller = Caller {
            pid: 1,
            uid: 999,
            gid: 999,
            client_kind: devcoordinator2_api::ClientKind::Edge,
            client_session: None,
            work: None,
            identity: Some("fixture@example.test".into()),
        };
        let world = Self {
            fixture,
            service,
            params,
            caller,
        };
        if world.params.kind == Kind::WebDeployment {
            world.configure_web();
        }
        world
    }

    fn configure_web(&self) {
        let spec =
            json!({"components":[{"name":"web","type":"process","route":true,"wants_port":true}]});
        let fingerprint = DeploymentStore::fingerprint(
            &json!({"spec":spec,"commit":"c".repeat(40),"dirty":false,"source_digest":"a".repeat(64)}),
        );
        let created = timestamp(FINISHED - 2000).unwrap();
        self.fixture.database.transaction(move |transaction| {
            transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,current_generation,created_at,created_by_uid,client,updated_at,public) VALUES('d1111111111111111','project-alpha','w1111111111111111','web','worktree',?1,?2,'running',1,?3,999,'fixture',?3,0)",rusqlite::params![fingerprint,spec.to_string(),created])?;
            transaction.execute("INSERT INTO generations VALUES('d1111111111111111',1,?1,0,'/fixture/web',?2,?3,'current')",rusqlite::params!["c".repeat(40),fingerprint,created])?;
            transaction.execute("INSERT INTO port_assignments VALUES(24002,'d1111111111111111','web',1,?1)",[created])?;
            Ok(())
        }).unwrap();
    }
}

fn proof(kind: Kind) -> Verification {
    let deployment = (kind == Kind::WebDeployment).then(|| WebDeploymentVerification {
        deployment_id: "d1111111111111111".into(),
        generation_number: 1,
        http_status: 200,
        content_type: "text/html; charset=utf-8".into(),
    });
    let (access, observation) = match kind {
        Kind::Artifact => (
            "https://downloads.example.test/v1/cli".into(),
            VerificationObservation::DownloadMatched,
        ),
        Kind::RegistryPackage => (
            "https://registry.example.test/project/-/project-1.0.0.tgz".into(),
            VerificationObservation::RegistryDownloadMatched,
        ),
        Kind::LocalExecutable => (
            format!("artifact://w1111111111111111/{RUN}/build/package/cli"),
            VerificationObservation::ExecutableSmokePassed,
        ),
        Kind::WebDeployment => (
            "http://127.0.0.1:24002/airfoils/ag24".into(),
            VerificationObservation::WebRoutePassed,
        ),
    };
    Verification {
        version: 1,
        kind,
        target: "linux-cli".into(),
        source_sha256: "a".repeat(64),
        file: "cli".into(),
        observed_sha256: digest(b"fixture executable bytes"),
        checked_at_ms: FINISHED - 100,
        access,
        observation,
        deployment,
    }
}

#[test]
fn delivery_pending_external_evidence_never_completes_release() {
    let world = World::new(Kind::RegistryPackage, None);
    let receipt = world
        .service
        .deliver(
            world.params.clone(),
            &world.caller,
            "fixture",
            FINISHED + 500,
        )
        .unwrap();
    assert_eq!(
        (
            receipt.qualified,
            receipt.verified_at_ms,
            receipt.delivered_at_ms,
            receipt.qualification
        ),
        (false, None, None, Qualification::PendingExternalEvidence)
    );
    let status = world
        .fixture
        .database
        .call(|connection| {
            Ok(connection.query_row(
                "SELECT status,delivered_at FROM releases WHERE release_id='release-alpha'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(status, ("planned".into(), None));
}

#[test]
fn delivery_all_kinds_preserve_actual_time_and_exact_receipts() {
    for kind in [
        Kind::Artifact,
        Kind::RegistryPackage,
        Kind::LocalExecutable,
        Kind::WebDeployment,
    ] {
        let world = World::new(kind.clone(), Some(proof(kind.clone())));
        let first = world
            .service
            .deliver(
                world.params.clone(),
                &world.caller,
                "fixture",
                FINISHED + 500,
            )
            .unwrap();
        assert!(first.qualified);
        assert_eq!(
            (
                first.repository_id.as_str(),
                first.target.as_str(),
                first.source_sha256.as_str(),
                first.verified_at_ms
            ),
            (
                "project-alpha",
                "linux-cli",
                "a".repeat(64).as_str(),
                Some(FINISHED - 100)
            )
        );
        assert_eq!(
            world
                .service
                .receipt(Reference {
                    reference: first.receipt_id.clone()
                })
                .unwrap(),
            first
        );
        assert_eq!(
            world
                .service
                .deliver(
                    world.params,
                    &world.caller,
                    "fixture",
                    FINISHED + 86_400_000
                )
                .unwrap(),
            first
        );
        let reviews: i32 = world
            .fixture
            .database
            .call(|connection| {
                Ok(connection
                    .query_row("SELECT COUNT(*) FROM review_records", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(reviews, 0);
    }
}

#[test]
fn delivery_rejects_unverified_flag_and_mismatched_retained_observations() {
    let world = World::new(Kind::RegistryPackage, Some(proof(Kind::RegistryPackage)));
    let mut forged = serde_json::to_value(&world.params).unwrap();
    forged["verified"] = json!(true);
    assert!(serde_json::from_value::<Deliver>(forged).is_err());
    for mode in 0..6 {
        let mut observation = proof(Kind::RegistryPackage);
        match mode {
            0 => observation.source_sha256 = "c".repeat(64),
            1 => observation.target = "macos-cli".into(),
            2 => observation.observed_sha256 = "d".repeat(64),
            3 => observation.checked_at_ms = FINISHED + 1,
            4 => observation.checked_at_ms = START,
            5 => observation.access = "https://owner:secret@registry.example.test/pkg".into(),
            _ => unreachable!(),
        }
        let world = World::new(Kind::RegistryPackage, Some(observation));
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err(),
            "mode {mode}"
        );
    }
}

#[test]
fn delivery_rejects_failed_tampered_wrong_source_and_wrong_repository_evidence() {
    for mode in 0..4 {
        let mut world = World::new(Kind::Artifact, Some(proof(Kind::Artifact)));
        match mode {
            0 => {
                let path = world.fixture.repository.join(".devcoordinator/test/logs/runs").join(RUN).join("run.json");
                let mut run: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                run["status"] = json!("failed");
                std::fs::write(path, serde_json::to_vec(&run).unwrap()).unwrap();
            }
            1 => std::fs::write(world.fixture.repository.join(".devcoordinator/test/logs/runs").join(RUN).join("checks/build/check/evidence/retained/package/cli"), b"tampered").unwrap(),
            2 => world.params.source_sha256 = "b".repeat(64),
            3 => world.fixture.database.call(|connection| {
                connection.execute_batch("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('other-project','/other','Other','t',1,'t'); UPDATE releases SET repository_id='other-project' WHERE release_id='release-alpha';")?;
                Ok(())
            }).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            world
                .service
                .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
                .is_err(),
            "mode {mode}"
        );
    }
}

#[test]
fn delivery_receipt_reads_are_bounded_and_persisted_without_mutation() {
    let world = World::new(Kind::Artifact, None);
    let receipt = world
        .service
        .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
        .unwrap();
    assert_eq!(
        world
            .service
            .show(Show {
                release_id: "release-alpha".into(),
                offset: 0,
                limit: 1
            })
            .unwrap()
            .receipts,
        vec![receipt]
    );
    assert!(
        world
            .service
            .show(Show {
                release_id: "release-alpha".into(),
                offset: 0,
                limit: 11
            })
            .is_err()
    );
    assert!(
        world
            .fixture
            .database
            .call(|connection| Ok(connection.execute("DELETE FROM release_evidence", [])?))
            .is_err()
    );
    assert!(
        world
            .service
            .receipt(Reference {
                reference: "fabricated".into()
            })
            .is_err()
    );
}

#[test]
fn delivery_parent_cli_and_protocol_getter_returns_qualified_receipt() {
    use crate::automation_test_support::FixtureClock;
    use crate::cli::{Cli, Invocation};
    use crate::control_plane::ControlPlane;
    use crate::daemon::{App, PeerCredentials};
    use clap::Parser;
    use devcoordinator2_api::{ClientContext, RequestEnvelope, ResponseEnvelope};
    use std::sync::Arc;

    let world = World::new(Kind::Artifact, Some(proof(Kind::Artifact)));
    let receipt = world
        .service
        .deliver(world.params, &world.caller, "fixture", FINISHED + 500)
        .unwrap();
    let invocation = Cli::try_parse_from([
        "devcoordinator2",
        "release",
        "evidence",
        &receipt.receipt_id,
        "--format",
        "json",
    ])
    .unwrap()
    .into_invocation()
    .unwrap();
    let Invocation::Remote { operation, params } = invocation else {
        panic!("read-only evidence invocation expected")
    };
    let tool = devcoordinator2_api::mcp_tool("release_evidence").unwrap();
    assert!(tool.operation.policy.read_only());
    (tool.validate_params)(&params).unwrap();
    let plane = ControlPlane::with_adapters(
        world.fixture.config.clone(),
        world.fixture.database.clone(),
        Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
        Arc::new(FixtureClock),
    )
    .unwrap();
    let app = App::with_executor(None, Arc::new(plane));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let response = runtime.block_on(app.dispatch(
        RequestEnvelope {
            protocol: 2,
            id: "evidence".into(),
            operation: operation.into(),
            params,
            client: ClientContext::default(),
        },
        PeerCredentials {
            pid: 1,
            uid: 1000,
            gid: 1000,
        },
    ));
    let ResponseEnvelope::Success { data, .. } = response else {
        panic!("evidence response: {response:?}")
    };
    assert_eq!(data, serde_json::to_value(receipt).unwrap());
    assert_eq!(data["qualified"], true);
    assert_eq!(data["verified_at_ms"], FINISHED - 100);
}
