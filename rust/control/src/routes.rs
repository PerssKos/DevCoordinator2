//! Complete, checksummed, atomically published edge route document.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devcoordinator2_api::params::AccessRole;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rustix::fs::{AtFlags, Mode, OFlags, open, openat, renameat, unlinkat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{PrimitiveDateTime, format_description::FormatItem, macros::format_description};

use crate::access::RoutePublisher;
pub use crate::access::{RouteAccessSection as RouteAccess, RouteGrant};
use crate::database::{Database, DatabaseError};
use crate::ids;
use crate::platform::{Clock, HostClock};

pub const ROUTE_SCHEMA: u8 = 2;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

fn timestamp_generation(now: &str) -> Result<u64, RouteError> {
    let timestamp = PrimitiveDateTime::parse(now, TIMESTAMP_FORMAT)?.assume_utc();
    u64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| RouteError::TimestampOutOfRange)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub deployment_id: String,
    pub component: String,
    pub label: String,
    pub domain: String,
    pub port: u16,
    pub scheme: String,
    pub auth: String,
    pub generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub observed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteDocument {
    pub schema: u8,
    pub payload_sha256: String,
    pub generation: u64,
    pub published_at: String,
    pub domain: String,
    pub routes: Vec<Route>,
    pub access: RouteAccess,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct RoutePayload<'a> {
    generation: u64,
    published_at: &'a str,
    domain: &'a str,
    routes: &'a [Route],
    access: &'a RouteAccess,
}

/// Production access/route publication adapter. Access changes commit before
/// this publishes one complete replacement document; publication failure is
/// returned while the committed change remains queryable for safe retry.
#[derive(Clone)]
pub struct RouteFilePublisher {
    database: Database,
    path: Arc<PathBuf>,
    base_domain: Arc<str>,
    clock: Arc<dyn Clock>,
}

impl RouteFilePublisher {
    pub fn new(database: Database, path: PathBuf, base_domain: String) -> Self {
        Self::with_clock(database, path, base_domain, Arc::new(HostClock))
    }

    pub fn with_clock(
        database: Database,
        path: PathBuf,
        base_domain: String,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            database,
            path: Arc::new(path),
            base_domain: base_domain.into(),
            clock,
        }
    }

    pub fn publish_current(&self) -> Result<RouteDocument, ProtocolError> {
        self.publish_snapshot(None)
    }

    pub fn wait_for_edge(&self, edge_state: &Path) -> Result<(), ProtocolError> {
        let document: RouteDocument =
            serde_json::from_slice(&std::fs::read(self.path.as_ref()).map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    "published route document is unavailable",
                )
            })?)
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    "published route document is invalid",
                )
            })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    "cannot observe edge acknowledgement",
                )
            })?;
        runtime.block_on(async {
            let mut events =
                crate::socket_endpoint::SocketEvents::new(edge_state).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        "edge acknowledgement directory is unavailable; leases remain reserved",
                    )
                })?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Ok(file) = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(edge_state.join("routes.accepted.json"))
                    && file.metadata().is_ok_and(|m| m.is_file() && m.len() < 4096)
                    && let Ok(value) =
                        serde_json::from_reader::<_, serde_json::Value>(file.take(4096))
                    && value["generation"].as_u64().is_some_and(|g| {
                        g > document.generation
                            || (g == document.generation
                                && value["payload_sha256"].as_str()
                                    == Some(document.payload_sha256.as_str()))
                    })
                {
                    return Ok(());
                }
                if !matches!(
                    tokio::time::timeout_at(deadline, events.changed()).await,
                    Ok(Ok(()))
                ) {
                    return Err(ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        "edge has not acknowledged route withdrawal; port leases remain reserved",
                    ));
                }
            }
        })
    }

    fn publish_snapshot(
        &self,
        access: Option<RouteAccess>,
    ) -> Result<RouteDocument, ProtocolError> {
        let now = self
            .clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format route timestamp")
                    .with_detail(error.to_string())
            })?;
        publish(&self.database, &self.path, &self.base_domain, access, &now).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "route publication failed")
                .with_detail(error.to_string())
        })
    }
}

impl RoutePublisher for RouteFilePublisher {
    fn publish_access(&self, access: &RouteAccess) -> Result<(), ProtocolError> {
        self.publish_snapshot(Some(access.clone())).map(|_| ())
    }
}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error(transparent)]
    Database(#[from] DatabaseError),
    #[error("route publication failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("route serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("route timestamp is invalid: {0}")]
    Timestamp(#[from] time::error::Parse),
    #[error("route timestamp is outside the generation range")]
    TimestampOutOfRange,
    #[error("route does not match its immutable port lease")]
    LeaseMismatch,
    #[error("route publication path is invalid")]
    InvalidPath,
    #[error("cannot create route temporary identity: {0}")]
    Id(#[from] ids::IdError),
}

pub fn publish(
    database: &Database,
    path: &Path,
    base_domain: &str,
    access: Option<RouteAccess>,
    now: &str,
) -> Result<RouteDocument, RouteError> {
    let base_domain_owned = base_domain.to_owned();
    let timestamp_floor = timestamp_generation(now)?;
    let floor_path = path
        .parent()
        .ok_or(RouteError::InvalidPath)?
        .join("route-generation.floor");
    // A file lease serializes every publisher, including distinct adapters.
    // Reserve the high-water mark before publishing so a crash may skip a
    // revision but cannot reuse one already seen by the edge.
    let _publication_lock = publication_lock(&floor_path)?;
    let persisted_floor = read_generation_floor(&floor_path)?;
    let (generation, routes, stored_access) = database.transaction(move |connection| {
        let current: Option<String> = connection
            .query_row(
                "SELECT value FROM meta WHERE key='route_generation'",
                [],
                |row| row.get(0),
            )
            .ok();
        let generation = current
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .max(persisted_floor)
            .checked_add(1)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(|| DatabaseError::Domain(ProtocolError::new(ErrorCode::InternalError, "route generation exhausted")))?
            .max(timestamp_floor);
        connection.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('route_generation',?1)",
            [generation.to_string()],
        )?;
        let candidates = {
            let mut statement = connection.prepare("SELECT deployment_id,component,port,generation,lease_id FROM domain_routes WHERE port IS NOT NULL")?;
            statement.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,u16>(2)?, row.get::<_,Option<u32>>(3)?, row.get::<_,Option<String>>(4)?)))?.collect::<Result<Vec<_>,_>>()?
        };
        for (deployment, component, port, selected, lease) in candidates {
            if lease.is_none() || crate::ports::route_lease(connection, &deployment, &component, port, selected, lease.as_deref())?.is_none() {
                crate::ports::withdraw_conflict(connection, &deployment, &component)?;
            }
        }
        let mut routes = Vec::new();
        {
            let mut statement = connection.prepare(
                "SELECT r.domain,r.deployment_id,r.component,r.port,r.generation,r.lease_id,d.public FROM domain_routes r JOIN deployments d ON d.deployment_id=r.deployment_id WHERE r.port IS NOT NULL ORDER BY r.domain",
            )?;
            for row in statement.query_map([], |row| {
                route_from_row(row, &base_domain_owned, false)
            })? {
                routes.push(row?);
            }
        }
        {
            let mut statement = connection.prepare(
                "SELECT domain,observed_deployment_id,component,port,NULL,NULL,public FROM observed_routes WHERE port IS NOT NULL ORDER BY domain",
            )?;
            for row in statement.query_map([], |row| {
                route_from_row(row, &base_domain_owned, true)
            })? {
                routes.push(row?);
            }
        }
        routes.sort_by(|left, right| left.label.cmp(&right.label));
        let stored_access = match access {
            Some(access) => access,
            None => {
                let owners = {
                    let mut statement = connection.prepare(
                        "SELECT email FROM users WHERE administrator=1 ORDER BY email",
                    )?;
                    statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let grants = {
                    let mut statement = connection.prepare(
                        "SELECT u.email,g.deployment_id,g.role FROM grants g JOIN users u ON u.user_id=g.user_id ORDER BY u.email,g.deployment_id",
                    )?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    rows.into_iter()
                        .map(|(identity, deployment_id, role)| {
                            Ok(RouteGrant {
                                identity,
                                deployment_id,
                                role: route_role(&role)?,
                            })
                        })
                        .collect::<Result<Vec<_>, DatabaseError>>()?
                };
                RouteAccess { owners, grants }
            }
        };
        Ok((generation, routes, stored_access))
    })?;
    let payload = RoutePayload {
        generation,
        published_at: now,
        domain: base_domain,
        routes: &routes,
        access: &stored_access,
    };
    let canonical = serde_json::to_vec(&serde_json::to_value(&payload)?)?;
    let payload_sha256 = lower_hex(&Sha256::digest(&canonical));
    let document = RouteDocument {
        schema: ROUTE_SCHEMA,
        payload_sha256,
        generation,
        published_at: now.to_owned(),
        domain: base_domain.to_owned(),
        routes,
        access: stored_access,
    };
    write_generation_floor(&floor_path, generation)?;
    atomic_publish(path, &serde_json::to_vec_pretty(&document)?)?;
    Ok(document)
}

fn route_role(value: &str) -> Result<AccessRole, DatabaseError> {
    match value {
        "access" => Ok(AccessRole::Access),
        "viewer" => Ok(AccessRole::Viewer),
        "operator" => Ok(AccessRole::Operator),
        "administrator" => Ok(AccessRole::Administrator),
        _ => Err(DatabaseError::Domain(ProtocolError::new(
            ErrorCode::InternalError,
            "stored route grant role is invalid",
        ))),
    }
}

fn route_from_row(
    row: &rusqlite::Row<'_>,
    base_domain: &str,
    _observed: bool,
) -> rusqlite::Result<Route> {
    let label: String = row.get(0)?;
    Ok(Route {
        deployment_id: row.get(1)?,
        component: row.get(2)?,
        domain: if base_domain.is_empty() {
            label.clone()
        } else {
            format!("{label}.{base_domain}")
        },
        label,
        port: row.get(3)?,
        scheme: "http".into(),
        auth: if row.get::<_, i64>(6)? != 0 {
            "public".into()
        } else {
            "authenticated".into()
        },
        generation: row.get(4)?,
        lease_id: if _observed { None } else { row.get(5)? },
        observed: _observed,
    })
}

fn publication_lock(path: &Path) -> Result<File, RouteError> {
    let parent = path.parent().ok_or(RouteError::InvalidPath)?;
    std::fs::create_dir_all(parent)?;
    let directory = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    let file = openat(
        &directory,
        "route-publication.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(RouteError::InvalidPath);
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
}

fn read_generation_floor(path: &Path) -> Result<u64, RouteError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > 32 {
        return Err(RouteError::InvalidPath);
    }
    let mut value = String::new();
    file.take(33).read_to_string(&mut value)?;
    value.trim().parse().map_err(|_| RouteError::InvalidPath)
}

fn write_generation_floor(path: &Path, generation: u64) -> Result<(), RouteError> {
    atomic_publish_mode(path, generation.to_string().as_bytes(), 0o600)
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn atomic_publish(path: &Path, bytes: &[u8]) -> Result<(), RouteError> {
    atomic_publish_mode(path, bytes, 0o644)
}

fn atomic_publish_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), RouteError> {
    let parent = path.parent().ok_or(RouteError::InvalidPath)?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or(RouteError::InvalidPath)?;
    std::fs::create_dir_all(parent)?;
    let directory = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    rustix::fs::fchmod(&directory, Mode::from_raw_mode(0o755)).map_err(std::io::Error::from)?;
    let temporary = format!(".routes-{}", ids::bug_id()?);
    let result = (|| -> Result<(), RouteError> {
        let descriptor = openat(
            &directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;
        let mut file = File::from(descriptor);
        file.write_all(bytes)?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(mode)).map_err(std::io::Error::from)?;
        file.sync_all()?;
        renameat(&directory, temporary.as_str(), &directory, name).map_err(std::io::Error::from)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn publication_is_complete_checksummed_and_monotonic() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1','/x','x','t',1,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1','r1','/x','t','t')", [])?;
                transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at,public) VALUES('d1','r1','w1','web','worktree','app','f','{}','running','t',1,'other','t',0)", [])?;
                transaction.execute("UPDATE deployments SET current_generation=1 WHERE deployment_id='d1'", [])?;
                transaction.execute("INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,updated_at) VALUES('d1','api','process',0,'f','running','running','healthy',1,'t')", [])?;
                transaction.execute("INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(20001,'d1','api',0,'t','ltest')", [])?;
                transaction.execute("INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES('app','d1','api',20001,1,'t','ltest')", [])?;
                transaction.execute("INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES('idle','d1','api',NULL,1,'t',NULL)", [])?;
                Ok(())
            })
            .unwrap();
        let path = temporary.path().join("public/routes.json");
        let first = publish(
            &database,
            &path,
            "example.test",
            None,
            "2026-09-03T12:00:00Z",
        )
        .unwrap();
        assert!(first.generation > 0);
        assert_eq!(first.routes.len(), 1);
        assert_eq!(first.routes[0].domain, "app.example.test");
        let value = serde_json::to_value(&first).unwrap();
        let payload = serde_json::json!({
            "generation": value["generation"],
            "published_at": value["published_at"],
            "domain": value["domain"],
            "routes": value["routes"],
            "access": value["access"],
        });
        assert_eq!(
            first.payload_sha256,
            lower_hex(&Sha256::digest(serde_json::to_vec(&payload).unwrap()))
        );
        let second = publish(
            &database,
            &path,
            "example.test",
            None,
            "2026-09-03T12:01:00Z",
        )
        .unwrap();
        assert!(second.generation > first.generation);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("payload_sha256")
        );
    }

    #[test]
    fn database_and_clock_rollback_cannot_reuse_a_published_generation() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("authority.sqlite3")).unwrap();
        let path = tmp.path().join("public/routes.json");
        let first = publish(&db, &path, "example.test", None, "2026-09-14T00:00:00Z").unwrap();
        db.call(|c| {
            c.execute(
                "UPDATE meta SET value='234' WHERE key='route_generation'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let second = publish(&db, &path, "example.test", None, "2026-09-13T00:00:00Z").unwrap();
        assert_eq!(second.generation, first.generation + 1);
        assert!(second.generation > 1071);
        std::fs::write(
            path.parent().unwrap().join("route-generation.floor"),
            b"broken",
        )
        .unwrap();
        assert!(publish(&db, &path, "example.test", None, "2026-09-14T00:00:00Z").is_err());
    }

    #[test]
    fn concurrent_publishers_keep_file_and_floor_at_the_highest_generation() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("authority.sqlite3")).unwrap();
        let path = tmp.path().join("public/routes.json");
        let jobs = (0..8)
            .map(|_| {
                let db = db.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    publish(&db, &path, "example.test", None, "2026-09-14T00:00:00Z")
                        .unwrap()
                        .generation
                })
            })
            .collect::<Vec<_>>();
        let mut generations = jobs
            .into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>();
        generations.sort();
        generations.dedup();
        assert_eq!(generations.len(), 8);
        let doc: RouteDocument = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc.generation, *generations.last().unwrap());
    }

    #[test]
    fn publication_rejects_a_route_that_does_not_match_its_lease() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1','/x','x','t',1,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1','r1','/x','t','t')", [])?;
                transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at,public) VALUES('d1','r1','w1','web','worktree','app','f','{}','running','t',1,'other','t',0)", [])?;
                transaction.execute("UPDATE deployments SET current_generation=1 WHERE deployment_id='d1'", [])?;
                transaction.execute("INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,updated_at) VALUES('d1','api','process',0,'f','running','running','healthy',1,'t')", [])?;
                transaction.execute("INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(20001,'d1','api',0,'t','ltest')", [])?;
                transaction.execute("INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES('app','d1','api',20001,1,'t','wrong')", [])?;
                Ok(())
            })
            .unwrap();
        let document = publish(
            &database,
            &temporary.path().join("public/routes.json"),
            "example.test",
            None,
            "2026-09-03T12:00:00Z",
        )
        .unwrap();
        assert!(document.routes.is_empty());
        assert_eq!(
            database
                .call(|c| Ok(c.query_row(
                    "SELECT state FROM deployments WHERE deployment_id='d1'",
                    [],
                    |r| r.get::<_, String>(0)
                )?))
                .unwrap(),
            "degraded"
        );
    }
}
