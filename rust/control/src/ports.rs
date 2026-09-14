//! Transactionally unique host-port leases.

use std::collections::{BTreeMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

use rusqlite::OptionalExtension;
use thiserror::Error;

use crate::database::{Database, DatabaseError};
use crate::ids;

pub trait PortAvailability: Send + Sync + 'static {
    fn bindable(&self, port: u16) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostPortAvailability;

impl PortAvailability for HostPortAvailability {
    fn bindable(&self, port: u16) -> bool {
        host_bindable(port)
    }
}

#[derive(Debug, Error)]
pub enum PortError {
    #[error("no free port in {0}-{1}")]
    Exhausted(u16, u16),
    #[error(transparent)]
    Database(#[from] DatabaseError),
}

pub fn lease(
    database: &Database,
    port_range: (u16, u16),
    deployment_id: &str,
    component: &str,
    generation: u32,
    now: &str,
) -> Result<u16, PortError> {
    lease_with_availability(
        database,
        port_range,
        deployment_id,
        component,
        generation,
        now,
        &HostPortAvailability,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn lease_with_availability(
    database: &Database,
    port_range: (u16, u16),
    deployment_id: &str,
    component: &str,
    generation: u32,
    now: &str,
    availability: &dyn PortAvailability,
) -> Result<u16, PortError> {
    let available = (port_range.0..=port_range.1)
        .filter(|port| availability.bindable(*port))
        .collect::<HashSet<_>>();
    let deployment_id = deployment_id.to_owned();
    let component = component.to_owned();
    let now = now.to_owned();
    database
        .transaction(move |transaction| {
            if let Some(port) = transaction.query_row(
                "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=?3",
                rusqlite::params![deployment_id, component, generation],
                |row| row.get::<_, u16>(0),
            ).optional()? {
                return Ok(port);
            }
            let mut statement = transaction.prepare("SELECT port FROM port_assignments UNION SELECT port FROM observed_routes")?;
            let taken = statement
                .query_map([], |row| row.get::<_, u16>(0))?
                .collect::<Result<HashSet<_>, _>>()?;
            for port in port_range.0..=port_range.1 {
                if taken.contains(&port) || !available.contains(&port) {
                    continue;
                }
                let lease_id = ids::lease_id().map_err(|error| {
                    DatabaseError::Domain(
                        devcoordinator2_api::ProtocolError::new(
                            devcoordinator2_api::ErrorCode::InternalError,
                            "cannot create port lease identity",
                        )
                        .with_detail(error.to_string()),
                    )
                })?;
                transaction.execute(
                    "INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![port, deployment_id, component, generation, now, lease_id],
                )?;
                return Ok(port);
            }
            Err(DatabaseError::Domain(devcoordinator2_api::ProtocolError::new(
                devcoordinator2_api::ErrorCode::Busy,
                format!("no free port in {}-{}", port_range.0, port_range.1),
            )))
        })
        .map_err(|error| match error {
            DatabaseError::Domain(ref error) if error.code == devcoordinator2_api::ErrorCode::Busy => PortError::Exhausted(port_range.0, port_range.1),
            other => PortError::Database(other),
        })
}

pub fn adopt_stable(
    database: &Database,
    deployment_id: &str,
    component: &str,
    generation: u32,
) -> Result<Option<u16>, PortError> {
    if generation == 0 {
        return assigned(database, deployment_id, 0).map(|ports| ports.get(component).copied());
    }
    let deployment_id = deployment_id.to_owned();
    let component = component.to_owned();
    database
        .transaction(move |transaction| {
            let current = transaction
                .query_row(
                    "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=?3",
                    rusqlite::params![deployment_id, component, generation],
                    |row| row.get::<_, u16>(0),
                )
                .optional()?;
            let Some(port) = current else {
                return Ok(None);
            };
            let stable = transaction
                .query_row(
                    "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=0",
                    rusqlite::params![deployment_id, component],
                    |row| row.get::<_, u16>(0),
                )
                .optional()?;
            if let Some(stable) = stable {
                if stable != port {
                    return Err(DatabaseError::Domain(devcoordinator2_api::ProtocolError::new(
                        devcoordinator2_api::ErrorCode::DeploymentApplyFailed,
                        "routed component has conflicting stable and generation leases",
                    )));
                }
                transaction.execute(
                    "DELETE FROM port_assignments WHERE port=?1 AND generation=?2",
                    rusqlite::params![port, generation],
                )?;
            } else {
                transaction.execute(
                    "UPDATE port_assignments SET generation=0 WHERE port=?1 AND deployment_id=?2 AND component=?3 AND generation=?4",
                    rusqlite::params![port, deployment_id, component, generation],
                )?;
            }
            Ok(Some(port))
        })
        .map_err(PortError::from)
}

pub fn release(
    database: &Database,
    deployment_id: &str,
    generation: Option<u32>,
    component: Option<&str>,
) -> Result<(), PortError> {
    let deployment_id = deployment_id.to_owned();
    let component = component.map(str::to_owned);
    database
        .transaction(move |transaction| {
            match (generation, component) {
                (Some(generation), Some(component)) => {
                    transaction.execute(
                        "DELETE FROM port_assignments WHERE deployment_id=?1 AND generation=?2 AND component=?3",
                        rusqlite::params![deployment_id, generation, component],
                    )?;
                }
                (Some(generation), None) => {
                    transaction.execute(
                        "DELETE FROM port_assignments WHERE deployment_id=?1 AND generation=?2",
                        rusqlite::params![deployment_id, generation],
                    )?;
                }
                (None, Some(component)) => {
                    transaction.execute(
                        "DELETE FROM port_assignments WHERE deployment_id=?1 AND component=?2",
                        rusqlite::params![deployment_id, component],
                    )?;
                }
                (None, None) => {
                    transaction.execute(
                        "DELETE FROM port_assignments WHERE deployment_id=?1",
                        [&deployment_id],
                    )?;
                }
            }
            Ok(())
        })
        .map_err(PortError::from)
}

pub fn assigned(
    database: &Database,
    deployment_id: &str,
    generation: u32,
) -> Result<BTreeMap<String, u16>, PortError> {
    let deployment_id = deployment_id.to_owned();
    database
        .call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT component,port FROM port_assignments WHERE deployment_id=?1 AND generation=?2",
            )?;
            Ok(statement
                .query_map(rusqlite::params![deployment_id, generation], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u16>(1)?))
                })?
                .collect::<Result<BTreeMap<_, _>, _>>()?)
        })
        .map_err(PortError::from)
}

/// Resolve the only valid routed lease in the caller's transaction. Runtime
/// generations may change; the routed lease itself must be stable.
pub fn route_lease(
    connection: &rusqlite::Connection,
    deployment_id: &str,
    component: &str,
    port: u16,
    generation: Option<u32>,
    expected: Option<&str>,
) -> Result<Option<String>, DatabaseError> {
    Ok(connection.query_row(
        "SELECT p.lease_id FROM port_assignments p
         JOIN components c ON c.deployment_id=p.deployment_id AND c.name=p.component
         JOIN deployments d ON d.deployment_id=p.deployment_id
         WHERE p.deployment_id=?1 AND p.component=?2 AND p.port=?3 AND p.generation=0
           AND p.lease_id IS NOT NULL AND (?5 IS NULL OR p.lease_id=?5)
           AND c.state='running' AND c.health='healthy' AND c.generation IN (0,?4)
           AND (d.current_generation=?4 OR (d.state='applying' AND EXISTS(
             SELECT 1 FROM generations g WHERE g.deployment_id=d.deployment_id AND g.number=?4 AND g.state='candidate')))",
        rusqlite::params![deployment_id,component,port,generation,expected],
        |row| row.get(0),
    ).optional()?)
}

pub fn withdraw_conflict(
    connection: &rusqlite::Connection,
    deployment_id: &str,
    component: &str,
) -> Result<(), DatabaseError> {
    connection.execute(
        "UPDATE domain_routes SET port=NULL,lease_id=NULL WHERE deployment_id=?1",
        [deployment_id],
    )?;
    connection.execute(
        "UPDATE deployments SET state='degraded' WHERE deployment_id=?1",
        [deployment_id],
    )?;
    connection.execute("UPDATE components SET last_error='route_lease_conflict' WHERE deployment_id=?1 AND name=?2", rusqlite::params![deployment_id,component])?;
    Ok(())
}

fn host_bindable(port: u16) -> bool {
    [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .into_iter()
        .all(|address| TcpListener::bind(SocketAddrV4::new(address, port)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn database() -> (tempfile::TempDir, Database) {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1','/x','x','t',1,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1','r1','/x','t','t')", [])?;
                transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1','r1','w1','web','worktree','app','f','{}','running','t',1,'other','t')", [])?;
                Ok(())
            })
            .unwrap();
        (temporary, database)
    }

    #[test]
    fn skips_bound_ports_and_releases_exact_scope() {
        let (_temporary, database) = database();
        struct FixtureAvailability;
        impl PortAvailability for FixtureAvailability {
            fn bindable(&self, port: u16) -> bool {
                port != 40_000
            }
        }
        let first = lease_with_availability(
            &database,
            (40_000, 40_010),
            "d1",
            "api",
            1,
            "t",
            &FixtureAvailability,
        )
        .unwrap();
        let second = lease_with_availability(
            &database,
            (40_000, 40_010),
            "d1",
            "worker",
            1,
            "t",
            &FixtureAvailability,
        )
        .unwrap();
        assert_eq!((first, second), (40_001, 40_002));
        assert_eq!(assigned(&database, "d1", 1).unwrap().len(), 2);
        release(&database, "d1", Some(1), Some("api")).unwrap();
        assert_eq!(
            assigned(&database, "d1", 1).unwrap(),
            BTreeMap::from([("worker".into(), 40_002)])
        );
    }

    #[test]
    fn adopts_a_generation_lease_as_the_stable_route_lease() {
        let (_temporary, database) = database();
        let port = lease_with_availability(
            &database,
            (40_000, 40_010),
            "d1",
            "api",
            7,
            "t",
            &HostPortAvailability,
        )
        .unwrap();
        assert_eq!(adopt_stable(&database, "d1", "api", 7).unwrap(), Some(port));
        assert_eq!(assigned(&database, "d1", 0).unwrap()["api"], port);
        assert!(assigned(&database, "d1", 7).unwrap().is_empty());
    }

    #[test]
    fn refuses_conflicting_stable_and_generation_leases() {
        let (_temporary, database) = database();
        lease_with_availability(
            &database,
            (40_000, 40_010),
            "d1",
            "api",
            0,
            "t",
            &HostPortAvailability,
        )
        .unwrap();
        lease_with_availability(
            &database,
            (40_001, 40_010),
            "d1",
            "api",
            7,
            "t",
            &HostPortAvailability,
        )
        .unwrap();
        assert!(adopt_stable(&database, "d1", "api", 7).is_err());
    }
}
