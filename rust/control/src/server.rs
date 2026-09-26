//! Server-wide adoption records for systemd-supervised external listeners.
//!
//! Production applications remain owned by systemd. These records let a
//! post-start gate publish the exact listener identity that current
//! Coordinator clients and route observers consume, without asking the daemon
//! to launch or supervise the application process.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use devcoordinator2_api::params::{ServerList as ServerListParams, ServerRegister, ServerStop};
use devcoordinator2_api::results::{
    ServerBroker, ServerHealth, ServerList, ServerPublication, ServerRow,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

use crate::database::{Database, DatabaseError};
use crate::ids;

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone)]
pub struct ServerService {
    database: Database,
}

impl ServerService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn register(&self, params: ServerRegister) -> Result<ServerRow, ProtocolError> {
        validate_register(&params)?;
        verify_listener(&params)?;
        let server_definition_id = stable_server_id(&params.agent, &params.project, &params.name);
        let lease_id = ids::lease_id().map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "could not create server lease")
                .with_detail(error.to_string())
        })?;
        let now = timestamp()?;
        let argv_json = serde_json::to_string(&params.argv).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "could not encode server argv")
                .with_detail(error.to_string())
        })?;
        let health_json = serde_json::to_string(&ServerHealth { ok: true }).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "could not encode server health")
                .with_detail(error.to_string())
        })?;
        let row_id = server_definition_id.clone();
        let agent = params.agent.clone();
        let project = params.project.clone();
        let name = params.name.clone();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO server_definitions(server_definition_id,agent,project,name,role,cwd,argv_json,pid,port,host,health_url,health_timeout,status,url_is_current,health_json,lease_id,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'running',1,?13,?14,COALESCE((SELECT created_at FROM server_definitions WHERE agent=?2 AND project=?3 AND name=?4),?15),?15) ON CONFLICT(agent,project,name) DO UPDATE SET server_definition_id=excluded.server_definition_id,role=excluded.role,cwd=excluded.cwd,argv_json=excluded.argv_json,pid=excluded.pid,port=excluded.port,host=excluded.host,health_url=excluded.health_url,health_timeout=excluded.health_timeout,status='running',url_is_current=1,health_json=excluded.health_json,lease_id=excluded.lease_id,updated_at=excluded.updated_at",
                    rusqlite::params![
                        row_id,
                        agent,
                        project,
                        name,
                        params.role,
                        params.cwd,
                        argv_json,
                        params.pid,
                        params.port,
                        params.host,
                        params.health_url,
                        params.health_timeout,
                        health_json,
                        lease_id,
                        now,
                    ],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        self.get_by_identity(&params.agent, &params.project, &params.name)
    }

    pub fn list(&self, params: ServerListParams) -> Result<ServerList, ProtocolError> {
        if params
            .project
            .as_deref()
            .is_some_and(|value| !Path::new(value).is_absolute())
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "server project must be an absolute path",
            ));
        }
        let project = params.project;
        let name = params.name;
        let rows = self
            .database
            .call(move |connection| {
                let mut sql = "SELECT server_definition_id,agent,project,name,role,cwd,argv_json,pid,port,host,health_url,health_timeout,status,url_is_current,health_json,lease_id FROM server_definitions".to_owned();
                let mut clauses = Vec::new();
                if project.is_some() {
                    clauses.push("project=?1");
                }
                if name.is_some() {
                    clauses.push(if project.is_some() { "name=?2" } else { "name=?1" });
                }
                if !clauses.is_empty() {
                    sql.push_str(" WHERE ");
                    sql.push_str(&clauses.join(" AND "));
                }
                sql.push_str(" ORDER BY project,name,agent");
                let mut statement = connection.prepare(&sql)?;
                let values = match (project.as_deref(), name.as_deref()) {
                    (Some(project), Some(name)) => vec![
                        rusqlite::types::Value::Text(project.to_owned()),
                        rusqlite::types::Value::Text(name.to_owned()),
                    ],
                    (Some(project), None) => {
                        vec![rusqlite::types::Value::Text(project.to_owned())]
                    }
                    (None, Some(name)) => vec![rusqlite::types::Value::Text(name.to_owned())],
                    (None, None) => Vec::new(),
                };
                statement
                    .query_map(rusqlite::params_from_iter(values.iter()), decode_row)
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        Ok(ServerList { servers: rows })
    }

    pub fn stop(&self, params: ServerStop) -> Result<ServerRow, ProtocolError> {
        let now = timestamp()?;
        let health_json = serde_json::to_string(&ServerHealth { ok: false }).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "could not encode server health")
                .with_detail(error.to_string())
        })?;
        let agent = params.agent.clone();
        let project = params.project.clone();
        let name = params.name.clone();
        let changed = self
            .database
            .call(move |connection| {
                let count = connection.execute(
                    "UPDATE server_definitions SET status='stopping',url_is_current=0,health_json=?1,updated_at=?2 WHERE agent=?3 AND project=?4 AND name=?5",
                    rusqlite::params![health_json, now, agent, project, name],
                )?;
                Ok(count)
            })
            .map_err(database_error)?;
        if changed == 0 {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentNotFound,
                "server registration was not found",
            ));
        }
        self.get_by_identity(&params.agent, &params.project, &params.name)
    }

    fn get_by_identity(
        &self,
        agent: &str,
        project: &str,
        name: &str,
    ) -> Result<ServerRow, ProtocolError> {
        let agent = agent.to_owned();
        let project = project.to_owned();
        let name = name.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT server_definition_id,agent,project,name,role,cwd,argv_json,pid,port,host,health_url,health_timeout,status,url_is_current,health_json,lease_id FROM server_definitions WHERE agent=?1 AND project=?2 AND name=?3",
                        rusqlite::params![agent, project, name],
                        decode_row,
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentNotFound,
                    "server registration was not found",
                )
            })
    }
}

fn validate_register(params: &ServerRegister) -> Result<(), ProtocolError> {
    if !Path::new(&params.project).is_absolute()
        || !Path::new(&params.cwd).is_absolute()
        || !Path::new(&params.project).is_dir()
        || !Path::new(&params.cwd).is_dir()
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "server project and cwd must be absolute paths",
        ));
    }
    if params.pid <= 1 || params.port == 0 || params.host != "127.0.0.1" {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "server pid, port, and host are invalid",
        ));
    }
    if !params.health_url.starts_with("http://127.0.0.1:") {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "server health URL must target loopback",
        ));
    }
    Ok(())
}

fn verify_listener(params: &ServerRegister) -> Result<(), ProtocolError> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(u64::from(params.health_timeout)))
        .build()
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "could not create server health client",
            )
            .with_detail(error.to_string())
        })?;
    let response = client.get(&params.health_url).send().map_err(|_| {
        ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health endpoint is unreachable",
        )
    })?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health endpoint did not return HTTP 200",
        ));
    }
    let mut body = Vec::new();
    response.take(4097).read_to_end(&mut body).map_err(|_| {
        ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health response could not be read",
        )
    })?;
    if body.len() > 4096 {
        return Err(ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health response is too large",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health response is not JSON",
        )
    })?;
    if value.get("status").and_then(serde_json::Value::as_str) != Some("ok") {
        return Err(ProtocolError::new(
            ErrorCode::DeploymentActionFailed,
            "registered server health response is not ready",
        ));
    }
    Ok(())
}

fn stable_server_id(agent: &str, project: &str, name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"devcoordinator2.server\0");
    digest.update(agent.as_bytes());
    digest.update([0]);
    digest.update(project.as_bytes());
    digest.update([0]);
    digest.update(name.as_bytes());
    format!(
        "s{}",
        digest
            .finalize()
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn timestamp() -> Result<String, ProtocolError> {
    OffsetDateTime::now_utc()
        .format(TIMESTAMP_FORMAT)
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "could not format server timestamp",
            )
            .with_detail(error.to_string())
        })
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServerRow> {
    let server_definition_id: String = row.get(0)?;
    let agent: String = row.get(1)?;
    let project: String = row.get(2)?;
    let name: String = row.get(3)?;
    let role: String = row.get(4)?;
    let cwd: String = row.get(5)?;
    let argv: Vec<String> = serde_json::from_str(&row.get::<_, String>(6)?).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            "invalid server argv".into(),
        )
    })?;
    let pid = row.get::<_, u32>(7)?;
    let port = row.get::<_, u16>(8)?;
    let host: String = row.get(9)?;
    let health_url: String = row.get(10)?;
    let health_timeout = row.get::<_, u16>(11)?;
    let status: String = row.get(12)?;
    let url_is_current = row.get::<_, i64>(13)? != 0;
    let health: ServerHealth = serde_json::from_str(&row.get::<_, String>(14)?).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            14,
            rusqlite::types::Type::Text,
            "invalid server health".into(),
        )
    })?;
    let lease_id: String = row.get(15)?;
    let running = status == "running";
    let publication = ServerPublication {
        lifecycle: if running { "running" } else { "stopped" }.into(),
        pid,
        port,
        server_definition_id: server_definition_id.clone(),
    };
    Ok(ServerRow {
        server_definition_id,
        agent,
        project,
        name,
        role,
        cwd,
        argv,
        pid,
        port,
        host,
        health_url,
        health_timeout,
        status,
        url_is_current,
        health,
        lease_id,
        broker: ServerBroker {
            status: if running { "active" } else { "stopped" }.into(),
            publication,
        },
    })
}

fn database_error(error: DatabaseError) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, "server registry operation failed")
        .with_detail(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn register() -> ServerRegister {
        ServerRegister {
            agent: "systemd-agent".into(),
            project: "/srv/example".into(),
            name: "production-web".into(),
            role: "gateway".into(),
            cwd: "/srv/example".into(),
            argv: vec!["npm".into(), "start".into()],
            pid: 42,
            port: 3001,
            host: "127.0.0.1".into(),
            health_url: "http://127.0.0.1:3001/healthz".into(),
            health_timeout: 5,
        }
    }

    #[test]
    fn register_is_idempotent_and_stop_invalidates_inventory() {
        let directory = tempdir().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let health_url = format!("http://127.0.0.1:{port}/healthz");
        let health_thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut request);
                std::io::Write::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}",
                )
                .unwrap();
            }
        });
        let database = Database::open(directory.path().join("authority.sqlite3")).unwrap();
        let service = ServerService::new(database.clone());
        let mut first_params = register();
        first_params.project = directory.path().display().to_string();
        first_params.cwd = first_params.project.clone();
        first_params.pid = std::process::id();
        first_params.port = port;
        first_params.health_url = health_url;
        let first = service.register(first_params.clone()).unwrap();
        assert_eq!(first.status, "running");
        assert!(first.url_is_current);
        assert_eq!(first.pid, std::process::id());
        let mut changed = first_params.clone();
        changed.pid = 43;
        let second = service.register(changed).unwrap();
        assert_eq!(first.server_definition_id, second.server_definition_id);
        assert_ne!(first.lease_id, second.lease_id);
        assert_eq!(
            service
                .list(ServerListParams::default())
                .unwrap()
                .servers
                .len(),
            1
        );
        let stopped = service
            .stop(ServerStop {
                agent: "systemd-agent".into(),
                project: first_params.project.clone(),
                name: "production-web".into(),
            })
            .unwrap();
        assert_eq!(stopped.status, "stopping");
        assert!(!stopped.url_is_current);
        assert!(!stopped.health.ok);
        health_thread.join().unwrap();
        database.close().unwrap();
    }
}
