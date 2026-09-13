use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use devcoordinator2_api::{ClientKind, RequestEnvelope};
use devcoordinator2_control::access::{Caller, RouteAccessSection};
use devcoordinator2_control::config::Config;
use devcoordinator2_control::control_plane::ControlPlane;
use devcoordinator2_control::daemon::OperationExecutor;
use devcoordinator2_control::database::Database;
use devcoordinator2_control::platform::HostClock;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("an unused external fixture directory is required")?,
    );
    std::fs::create_dir(&root)?;
    std::fs::write(
        root.join("glossary-fixture.marker"),
        "isolated glossary acceptance fixture",
    )?;
    let database = Database::open(root.join("authority.sqlite3"))?;
    database.transaction(|connection| {
        for (identity, name) in [("r1111111111111111", "Vocabulary project"), ("r2222222222222222", "Other project")] {
            connection.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,?2,?3,'now',1000,'now')", rusqlite::params![identity, format!("/fixture/{identity}"), name])?;
        }
        connection.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/fixture/project','now','now')", [])?;
        connection.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1111111111111111','r1111111111111111','w1111111111111111','fixture','permanent','fixture','{}','stopped','now',1000,'other','now')", [])?;
        for (identity, email, admin) in [("u1111111111111111", "owner@example.test", 1), ("u2222222222222222", "viewer@example.test", 0)] {
            connection.execute("INSERT INTO users(user_id,email,administrator,created_at,created_by) VALUES(?1,?2,?3,'now','fixture')", rusqlite::params![identity, email, admin])?;
        }
        connection.execute("INSERT INTO grants VALUES('u2222222222222222','d1111111111111111','viewer','now','fixture')", [])?;
        Ok(())
    })?;
    let config = Config {
        socket_path: root.join("daemon.sock"),
        state_dir: root.join("state"),
        unit_prefix: "devcoordinator2-glossary-fixture".into(),
        slice_name: "unused-fixture.slice".into(),
        client_group: "unused-fixture".into(),
        port_range: (40000, 40100),
        base_domain: "example.test".into(),
        edge_uid: Some(999),
        admin_emails: Vec::new(),
        telegram_token_file: None,
        telegram_api: "https://api.telegram.org".into(),
        bugs_dir: root.join("bugs"),
        compose_env_allowlist_file: None,
        compose_env_authorizations: HashSet::new(),
        codex_usage_sources_file: None,
        codex_usage_sources: Vec::new(),
    };
    let plane = ControlPlane::with_adapters(
        config,
        database.clone(),
        Arc::new(|_: &RouteAccessSection| Ok(())),
        Arc::new(HostClock),
    )?;
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let request: RequestEnvelope = serde_json::from_str(&line?)?;
        let public = request.client.identity.is_some();
        let caller = Caller {
            pid: 1,
            uid: if public { 999 } else { 1000 },
            gid: 1000,
            client_kind: if public {
                ClientKind::Edge
            } else {
                ClientKind::Other
            },
            client_session: None,
            work: None,
            identity: request.client.identity,
        };
        let response = match plane.execute(&request.operation, request.params, &caller) {
            Ok(data) => json!({"protocol":2,"id":request.id,"ok":true,"data":data}),
            Err(error) => {
                json!({"protocol":2,"id":request.id,"ok":false,"error":{"code":error.code,"message":error.message,"detail":""}})
            }
        };
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    drop(plane);
    database.close()?;
    Ok(())
}
