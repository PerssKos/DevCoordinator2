//! Single-owner SQLite actor for permanent Coordinator authority state.

use std::any::Any;
use std::fs::{OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::DATABASE_SCHEMA_VERSION;

const FINAL_SCHEMA: &str = include_str!("schema.sql");

type ActorResult = Result<Box<dyn Any + Send>, DatabaseError>;
type Work = Box<dyn FnOnce(&mut Connection) -> ActorResult + Send>;

enum Message {
    Work(Work),
    Stop,
}

#[derive(Clone)]
pub struct Database {
    sender: mpsc::Sender<Message>,
}

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("database actor is unavailable")]
    ActorUnavailable,
    #[error("database schema {found} is newer than daemon {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("database returned an unexpected result type")]
    ResultType,
    #[error(transparent)]
    Domain(#[from] devcoordinator2_api::ProtocolError),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database actor failed to start: {0}")]
    Startup(String),
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let path = path.as_ref().to_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| DatabaseError::Startup(error.to_string()))?;
        }
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("devcoordinator2-sqlite".to_owned())
            .spawn(move || actor(path, receiver, ready_sender))
            .map_err(|error| DatabaseError::Startup(error.to_string()))?;
        ready_receiver
            .recv()
            .map_err(|_| DatabaseError::ActorUnavailable)??;
        Ok(Self { sender })
    }

    pub fn call<T, F>(&self, work: F) -> Result<T, DatabaseError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, DatabaseError> + Send + 'static,
    {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(Message::Work(Box::new(move |connection| {
                let result = work(connection).map(|value| Box::new(value) as Box<dyn Any + Send>);
                let _ = result_sender.send(result);
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })))
            .map_err(|_| DatabaseError::ActorUnavailable)?;
        result_receiver
            .recv()
            .map_err(|_| DatabaseError::ActorUnavailable)??
            .downcast::<T>()
            .map(|value| *value)
            .map_err(|_| DatabaseError::ResultType)
    }

    pub fn transaction<T, F>(&self, work: F) -> Result<T, DatabaseError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, DatabaseError> + Send + 'static,
    {
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let result = work(&transaction)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn backup(&self, destination: PathBuf) -> Result<(), DatabaseError> {
        self.call(move |connection| {
            // Create privately before SQLite writes any authority data.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&destination)
                .map_err(|error| DatabaseError::Startup(error.to_string()))?;
            connection.backup(rusqlite::MAIN_DB, &destination, None)?;
            Ok(())
        })
    }

    pub fn close(&self) -> Result<(), DatabaseError> {
        self.sender
            .send(Message::Stop)
            .map_err(|_| DatabaseError::ActorUnavailable)
    }
}

fn actor(
    path: PathBuf,
    receiver: mpsc::Receiver<Message>,
    ready: mpsc::SyncSender<Result<(), DatabaseError>>,
) {
    let mut connection = match open_connection(&path) {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            connection
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Work(work) => {
                let _ = work(&mut connection);
            }
            Message::Stop => break,
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection, DatabaseError> {
    // SQLite inherits the database mode for newly created journals. Repair old
    // sidecars before opening too, since reopening does not change their modes.
    for (path, create) in
        std::iter::once((path.to_owned(), true)).chain(["-wal", "-shm", "-journal"].map(|suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            (PathBuf::from(name), false)
        }))
    {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .map_err(|error| DatabaseError::Startup(error.to_string()))?;
                if !metadata.is_file() {
                    return Err(DatabaseError::Startup(
                        "authority state must be a regular file".to_owned(),
                    ));
                }
                file.set_permissions(Permissions::from_mode(0o600))
                    .map_err(|error| DatabaseError::Startup(error.to_string()))?;
            }
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(DatabaseError::Startup(error.to_string())),
        }
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    let connection = Connection::open_with_flags(path, flags)?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
    )?;
    let current = connection
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    if let Some(found) = current
        && found > DATABASE_SCHEMA_VERSION
    {
        return Err(DatabaseError::SchemaTooNew {
            found,
            supported: DATABASE_SCHEMA_VERSION,
        });
    }
    connection.execute_batch(FINAL_SCHEMA)?;
    ensure_column(
        &connection,
        "deployments",
        "public",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(&connection, "deployments", "domain_override", "TEXT")?;
    relax_observed_state_checks(&connection)?;
    ensure_column(
        &connection,
        "tasks",
        "elaboration_needed",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(&connection, "repositories", "archived_at", "TEXT")?;
    ensure_column(&connection, "repositories", "archived_by_uid", "INTEGER")?;
    ensure_column(&connection, "repositories", "archive_note", "TEXT")?;
    ensure_column(
        &connection,
        "repositories",
        "merged_into_repository_id",
        "TEXT",
    )?;
    ensure_column(&connection, "port_assignments", "lease_id", "TEXT")?;
    ensure_column(&connection, "domain_routes", "lease_id", "TEXT")?;
    backfill_lease_ids(&connection)?;
    connection.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
        [DATABASE_SCHEMA_VERSION.to_string()],
    )?;
    Ok(connection)
}

fn backfill_lease_ids(connection: &Connection) -> Result<(), DatabaseError> {
    let transaction = connection.unchecked_transaction()?;
    let connection = &transaction;
    let assignments = {
        let mut statement = connection.prepare(
            "SELECT port,deployment_id,component,generation FROM port_assignments WHERE lease_id IS NULL OR lease_id=''",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u16>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (port, deployment_id, component, generation) in assignments {
        let mut digest = Sha256::new();
        digest.update(b"devcoordinator2.lease\0");
        digest.update(deployment_id.as_bytes());
        digest.update([0]);
        digest.update(component.as_bytes());
        digest.update([0]);
        digest.update(generation.to_le_bytes());
        digest.update(port.to_le_bytes());
        let lease_id = format!(
            "l{}",
            digest
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        connection.execute(
            "UPDATE port_assignments SET lease_id=?1 WHERE port=?2",
            rusqlite::params![lease_id, port],
        )?;
    }
    let routes = {
        let mut statement = connection.prepare(
            "SELECT domain,deployment_id,component,port,generation FROM domain_routes WHERE port IS NOT NULL AND (lease_id IS NULL OR lease_id='')",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (domain, deployment_id, component, port, generation) in routes {
        let lease_id: Option<String> = connection
            .query_row(
                "SELECT lease_id FROM port_assignments WHERE port=?1 AND deployment_id=?2 AND component=?3 AND generation IN (?4,0) ORDER BY CASE WHEN generation=?4 THEN 0 ELSE 1 END LIMIT 1",
                rusqlite::params![port, deployment_id, component, generation.unwrap_or(0)],
                |row| row.get(0),
            )
            .optional()?;
        let conflicting: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation=0 AND port!=?3)",
            rusqlite::params![deployment_id,component,port], |row| row.get(0),
        )?;
        if let Some(lease_id) = lease_id.filter(|_| !conflicting) {
            connection.execute(
                "UPDATE port_assignments SET generation=0 WHERE lease_id=?1",
                [&lease_id],
            )?;
            connection.execute(
                "UPDATE domain_routes SET lease_id=?1 WHERE domain=?2",
                rusqlite::params![lease_id, domain],
            )?;
        } else {
            connection.execute(
                "UPDATE domain_routes SET port=NULL,lease_id=NULL WHERE domain=?1",
                [domain],
            )?;
            connection.execute(
                "UPDATE deployments SET state='degraded' WHERE deployment_id=?1",
                [&deployment_id],
            )?;
            connection.execute("UPDATE components SET last_error='route_lease_conflict' WHERE deployment_id=?1 AND name=?2", rusqlite::params![deployment_id,component])?;
        }
    }
    connection.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS port_lease_identity ON port_assignments(lease_id);
        CREATE TRIGGER IF NOT EXISTS immutable_port_lease BEFORE UPDATE OF lease_id,port,deployment_id,component ON port_assignments
        WHEN OLD.lease_id IS NOT NULL AND (NEW.lease_id IS NOT OLD.lease_id OR NEW.port IS NOT OLD.port OR NEW.deployment_id IS NOT OLD.deployment_id OR NEW.component IS NOT OLD.component)
        BEGIN SELECT RAISE(ABORT,'immutable port lease'); END;
        CREATE TRIGGER IF NOT EXISTS unique_lease_owner BEFORE INSERT ON port_assignments
        WHEN NEW.lease_id IS NULL OR EXISTS(SELECT 1 FROM port_assignments WHERE deployment_id=NEW.deployment_id AND component=NEW.component AND generation=NEW.generation)
        BEGIN SELECT RAISE(ABORT,'missing or duplicate port lease identity'); END;")?;
    transaction.commit()?;
    Ok(())
}

fn relax_observed_state_checks(connection: &Connection) -> Result<(), DatabaseError> {
    let sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='observed_deployments'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    if sql
        .as_deref()
        .is_none_or(|value| value.contains("'stopped'"))
    {
        return Ok(());
    }
    connection.execute_batch(
        "PRAGMA foreign_keys=OFF;
         PRAGMA legacy_alter_table=ON;
         ALTER TABLE observed_deployments RENAME TO observed_deployments_v6;
         ALTER TABLE observed_containers RENAME TO observed_containers_v6;
         CREATE TABLE observed_deployments (
           observed_deployment_id TEXT PRIMARY KEY,
           repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
           name TEXT NOT NULL,
           native_project TEXT NOT NULL UNIQUE,
           state TEXT NOT NULL CHECK(state IN ('running','degraded','stopped','failed')),
           health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','unknown')),
           source TEXT NOT NULL,
           evidence_json TEXT NOT NULL,
           observed_at TEXT NOT NULL,
           imported_at TEXT NOT NULL,
           UNIQUE(repository_id,native_project)
         );
         CREATE TABLE observed_containers (
           container_id TEXT PRIMARY KEY,
           observed_deployment_id TEXT NOT NULL REFERENCES observed_deployments(observed_deployment_id) ON DELETE CASCADE,
           repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
           name TEXT NOT NULL,
           image TEXT NOT NULL,
           compose_service TEXT NOT NULL,
           state TEXT NOT NULL CHECK(state IN ('running','stopped','failed','starting','missing')),
           status TEXT NOT NULL,
           health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','starting','unknown','none')),
           observed_at TEXT NOT NULL
         );
         INSERT INTO observed_deployments SELECT * FROM observed_deployments_v6;
         INSERT INTO observed_containers SELECT * FROM observed_containers_v6;
         DROP TABLE observed_containers_v6;
         DROP TABLE observed_deployments_v6;
         CREATE INDEX IF NOT EXISTS observed_containers_deployment ON observed_containers(observed_deployment_id);
         CREATE INDEX IF NOT EXISTS observed_containers_repository ON observed_containers(repository_id);
         PRAGMA legacy_alter_table=OFF;
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), DatabaseError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|candidate| candidate == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_schema_sixteen_with_owned_events_and_serializes_calls() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("open");
        let version: String = database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT value FROM meta WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("version");
        assert_eq!(version, DATABASE_SCHEMA_VERSION.to_string());
        let tables: Vec<String> = database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name",
                )?;
                Ok(statement
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .expect("tables");
        for required in [
            "repositories",
            "deployments",
            "tasks",
            "decisions",
            "visual_feedback",
            "visual_feedback_comments",
            "visual_feedback_events",
            "owned_events",
        ] {
            assert!(
                tables.iter().any(|table| table == required),
                "missing {required}"
            );
        }
        database.close().expect("close");
    }

    #[test]
    fn schema_twenty_migration_preserves_lease_port_and_withdraws_conflicts() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("authority.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(FINAL_SCHEMA).unwrap();
        connection
            .execute_batch("ALTER TABLE domain_routes DROP COLUMN lease_id;")
            .unwrap();
        // The old table has no identity column or its unique index.
        connection.execute_batch("DROP TABLE port_assignments; CREATE TABLE port_assignments(port INTEGER PRIMARY KEY,deployment_id TEXT,component TEXT,generation INTEGER,assigned_at TEXT);
            INSERT INTO meta VALUES('schema_version','20');
            INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1','/x','x','t',1,'t');
            INSERT INTO worktrees VALUES('w1','r1','/x','t','t');
            INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,current_generation,created_at,created_by_uid,client,updated_at) VALUES('d1','r1','w1','app','worktree','f','{}','running',7,'t',1,'other','t');
            INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,updated_at) VALUES('d1','api','process',0,'f','running','running','healthy',7,'t');
            INSERT INTO port_assignments VALUES(20000,'d1','api',7,'t');
            INSERT INTO domain_routes VALUES('good','d1','api',20000,7,'t');").unwrap();
        drop(connection);
        let db = Database::open(&path).unwrap();
        let identity: String = db
            .call(|c| {
                let (port, generation, lease): (u16, u32, String) = c.query_row(
                    "SELECT port,generation,lease_id FROM port_assignments",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                assert_eq!((port, generation), (20000, 0));
                assert_eq!(
                    c.query_row("SELECT lease_id FROM domain_routes", [], |r| r
                        .get::<_, String>(0))?,
                    lease
                );
                assert!(
                    c.execute("UPDATE port_assignments SET port=20014", [])
                        .is_err()
                );
                Ok(lease)
            })
            .unwrap();
        db.call(|c| {
            c.execute("UPDATE domain_routes SET port=20014,lease_id=NULL", [])?;
            super::backfill_lease_ids(c)?;
            assert!(
                c.query_row("SELECT port FROM domain_routes", [], |r| r
                    .get::<_, Option<u16>>(0))?
                    .is_none()
            );
            assert_eq!(
                c.query_row("SELECT state FROM deployments", [], |r| r
                    .get::<_, String>(0))?,
                "degraded"
            );
            Ok(())
        })
        .unwrap();
        let retained = db
            .call(|c| {
                Ok(c.query_row(
                    "SELECT lease_id FROM port_assignments WHERE port=20000",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(identity, retained);
    }

    #[test]
    fn rejects_a_newer_schema() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("authority.sqlite3");
        let connection = Connection::open(&path).expect("fixture");
        let future_version = DATABASE_SCHEMA_VERSION + 1;
        connection
            .execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("fixture schema");
        connection
            .execute(
                "INSERT INTO meta VALUES('schema_version',?1)",
                [future_version.to_string()],
            )
            .expect("future schema");
        drop(connection);
        assert!(matches!(
            Database::open(path),
            Err(DatabaseError::SchemaTooNew {
                found,
                supported
            }) if found == future_version && supported == DATABASE_SCHEMA_VERSION
        ));
    }

    #[test]
    fn upgrades_schema_eighteen_with_separate_repository_presentation() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("authority.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO meta VALUES('schema_version','18');").unwrap();
        drop(connection);
        let database = Database::open(path).unwrap();
        let columns: Vec<String> = database
            .call(|connection| {
                let mut query = connection.prepare("PRAGMA table_info(repository_presentation)")?;
                Ok(query
                    .query_map([], |row| row.get(1))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap();
        assert_eq!(
            columns,
            [
                "repository_id",
                "display_name",
                "icon",
                "updated_at",
                "updated_by_uid"
            ]
        );
    }

    #[test]
    fn upgrades_schema_fifteen_by_adding_the_owned_event_journal() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("authority.sqlite3");
        let connection = Connection::open(&path).expect("fixture");
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);\n\
                 INSERT INTO meta VALUES('schema_version','15');",
            )
            .expect("fixture schema");
        drop(connection);

        let database = Database::open(path).expect("upgrade");
        let (version, owned_events): (String, i64) = database
            .call(|connection| {
                Ok((
                    connection.query_row(
                        "SELECT value FROM meta WHERE key='schema_version'",
                        [],
                        |row| row.get(0),
                    )?,
                    connection.query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='owned_events'",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            })
            .expect("schema projection");
        assert_eq!(version, DATABASE_SCHEMA_VERSION.to_string());
        assert_eq!(owned_events, 1);
    }

    #[test]
    fn backup_is_new_and_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("open");
        let backup = temporary.path().join("authority.backup.sqlite3");
        database.backup(backup.clone()).expect("backup");
        assert_eq!(
            std::fs::metadata(&backup)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(database.backup(backup).is_err());
    }

    #[test]
    fn authority_and_journals_are_private_on_creation_and_reopen() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempdir().unwrap();
        let path = temporary.path().join("authority.sqlite3");
        let connection = open_connection(&path).unwrap();
        let files = [
            path.clone(),
            path.with_extension("sqlite3-wal"),
            path.with_extension("sqlite3-shm"),
        ];
        for file in &files {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let reopened = open_connection(&path).unwrap();
        for file in &files {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(reopened);
        drop(connection);
        let _recreated = open_connection(&path).unwrap();
        for file in &files {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn upgrades_schema_six_observed_states_without_losing_rows() {
        let temporary = tempdir().expect("tempdir");
        let path = temporary.path().join("authority.sqlite3");
        let connection = Connection::open(&path).expect("fixture");
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 INSERT INTO meta VALUES('schema_version','6');
                 CREATE TABLE repositories(
                   repository_id TEXT PRIMARY KEY,root_path TEXT NOT NULL UNIQUE,
                   display_name TEXT NOT NULL,registered_at TEXT NOT NULL,
                   registered_by_uid INTEGER NOT NULL,last_seen_at TEXT NOT NULL);
                 INSERT INTO repositories VALUES('r1','/x','x','t',1,'t');
                 CREATE TABLE observed_deployments(
                   observed_deployment_id TEXT PRIMARY KEY,
                   repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
                   name TEXT NOT NULL,native_project TEXT NOT NULL UNIQUE,
                   state TEXT NOT NULL CHECK(state IN ('running','degraded')),
                   health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','unknown')),
                   source TEXT NOT NULL,evidence_json TEXT NOT NULL,
                   observed_at TEXT NOT NULL,imported_at TEXT NOT NULL,
                   UNIQUE(repository_id,native_project));
                 CREATE TABLE observed_containers(
                   container_id TEXT PRIMARY KEY,
                   observed_deployment_id TEXT NOT NULL REFERENCES observed_deployments(observed_deployment_id) ON DELETE CASCADE,
                   repository_id TEXT NOT NULL REFERENCES repositories(repository_id),
                   name TEXT NOT NULL,image TEXT NOT NULL,compose_service TEXT NOT NULL,
                   state TEXT NOT NULL CHECK(state IN ('running')),
                   status TEXT NOT NULL,
                   health TEXT NOT NULL CHECK(health IN ('healthy','unhealthy','starting','unknown')),
                   observed_at TEXT NOT NULL);
                 CREATE TABLE observed_routes(
                   domain TEXT PRIMARY KEY,
                   observed_deployment_id TEXT NOT NULL REFERENCES observed_deployments(observed_deployment_id) ON DELETE CASCADE,
                   component TEXT NOT NULL,port INTEGER NOT NULL,public INTEGER NOT NULL,
                   evidence_json TEXT NOT NULL,observed_at TEXT NOT NULL);
                 INSERT INTO observed_deployments VALUES('d1','r1','one','native','running','healthy','s','{}','t','t');
                 INSERT INTO observed_containers VALUES('c1','d1','r1','one','image','svc','running','up','healthy','t');
                 INSERT INTO observed_routes VALUES('one','d1','svc',1234,0,'{}','t');",
            )
            .expect("schema-six fixture");
        drop(connection);

        let database = Database::open(&path).expect("upgrade");
        database
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE observed_deployments SET state='stopped' WHERE observed_deployment_id='d1'",
                    [],
                )?;
                transaction.execute(
                    "UPDATE observed_containers SET state='stopped',health='none' WHERE container_id='c1'",
                    [],
                )?;
                Ok(())
            })
            .expect("new states");
        let routes: i64 = database
            .call(|connection| {
                Ok(connection
                    .query_row("SELECT count(*) FROM observed_routes", [], |row| row.get(0))?)
            })
            .expect("route count");
        assert_eq!(routes, 1);
    }
}
