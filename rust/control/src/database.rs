//! Synchronous domain access to the PostgreSQL authority database.
//!
//! Reads, writes and maintenance have separate bounded pools. There is no
//! single-owner actor and no writable SQLite authority fallback.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-fixtures"))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use devcoordinator2_persistence::Persistence;
#[cfg(any(test, feature = "test-fixtures"))]
use devcoordinator2_persistence::fixture::FixtureDatabase;
pub use devcoordinator2_persistence::maintenance;
pub use devcoordinator2_persistence::{
    Connection, DatabaseConfig, Error, FromValue, Lane, OptionalExtension, Params, PoolSizes,
    Result, Row, Rows, Statement, ToValue, Transaction, Value, params, params_from_iter, types,
};
use thiserror::Error;

thread_local! { static MAINTENANCE_CONTEXT:Cell<bool>=const {Cell::new(false)}; }

const OPTIONAL_TRANSACTION_BUDGET: Duration = Duration::from_millis(100);
const OPTIONAL_LOCK_BUDGET: Duration = Duration::from_millis(25);

/// Optional metadata shares one absolute transaction budget. Each statement
/// receives only the time remaining; domain mutation timeouts stay unchanged.
pub(crate) struct OptionalTransaction<'a, 'connection> {
    transaction: &'a Transaction<'connection>,
    deadline: Instant,
}

impl OptionalTransaction<'_, '_> {
    fn remaining(&self) -> std::result::Result<Duration, DatabaseError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                DatabaseError::Domain(devcoordinator2_api::ProtocolError::new(
                    devcoordinator2_api::ErrorCode::Busy,
                    "optional metadata transaction reached its deadline",
                ))
            })
    }

    /// Run one statement within the remaining metadata budget.
    pub(crate) fn run<T>(
        &self,
        work: impl FnOnce(&Connection) -> std::result::Result<T, DatabaseError>,
    ) -> std::result::Result<T, DatabaseError> {
        let remaining = self.remaining()?;
        self.transaction.execute_batch(&format!(
            "SET LOCAL statement_timeout={}; SET LOCAL lock_timeout={}",
            remaining.as_millis().clamp(1, i32::MAX as u128),
            remaining.min(OPTIONAL_LOCK_BUDGET).as_millis().max(1),
        ))?;
        let result = work(self.transaction)?;
        self.remaining()?;
        Ok(result)
    }
}
/// Route nested domain operations to maintenance capacity on this thread.
/// The guard restores the prior scope on success, failure and unwinding.
pub fn maintenance_context<T>(work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            MAINTENANCE_CONTEXT.with(|v| v.set(self.0));
        }
    }
    let previous = MAINTENANCE_CONTEXT.with(|v| v.replace(true));
    let _reset = Reset(previous);
    work()
}
fn request_lane() -> Lane {
    if MAINTENANCE_CONTEXT.with(Cell::get) {
        Lane::Maintenance
    } else {
        Lane::Write
    }
}

/// The native synchronous PostgreSQL client owns its own I/O runtime. Keep it
/// outside async task execution; current-thread test runtimes use a scoped
/// thread, while the production multithread runtime lends a blocking worker.
fn blocking<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    let maintenance = MAINTENANCE_CONTEXT.with(Cell::get);
    let work = move || {
        if maintenance {
            maintenance_context(work)
        } else {
            work()
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        Ok(_) => std::thread::scope(|scope| match scope.spawn(work).join() {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }),
        Err(_) => work(),
    }
}

#[derive(Clone)]
pub struct Database {
    // Pools must drop before the last fixture guard stops its owned cluster.
    inner: ManuallyDrop<Persistence>,
    #[cfg(any(test, feature = "test-fixtures"))]
    fixture: Option<Arc<FixtureDatabase>>,
}
impl Drop for Database {
    fn drop(&mut self) {
        // postgres::Client closes through its synchronous runtime as well.
        // Drop the final pool owner through the same async-safe boundary.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        blocking(move || drop(inner));
    }
}

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error(transparent)]
    Domain(#[from] devcoordinator2_api::ProtocolError),
    #[error(transparent)]
    Query(#[from] devcoordinator2_persistence::Error),
    /// Read-only legacy snapshots are migration input, never authority state.
    #[error("legacy snapshot could not be read: {0}")]
    LegacySnapshot(#[from] rusqlite::Error),
    #[error("database initialization failed: {0}")]
    Startup(String),
    #[error("database schema {found} is newer than daemon {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
}

impl Database {
    pub(crate) fn in_maintenance_context() -> bool {
        MAINTENANCE_CONTEXT.with(Cell::get)
    }

    /// Create an isolated real PostgreSQL fixture selected by a scratch path.
    /// Daemon startup must use `connect`, with explicit private configuration.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn open(path: impl AsRef<Path>) -> std::result::Result<Self, DatabaseError> {
        let path = path.as_ref().to_owned();
        blocking(move || {
            let fixture = Arc::new(FixtureDatabase::for_path(&path)?);
            let inner = Persistence::connect(&fixture.config)?;
            inner.migrate()?;
            Ok(Self {
                inner: ManuallyDrop::new(inner),
                fixture: Some(fixture),
            })
        })
    }
    pub fn connect(config: &DatabaseConfig) -> std::result::Result<Self, DatabaseError> {
        blocking(|| {
            let inner = Persistence::connect(config)?;
            inner.migrate()?;
            Ok(Self {
                inner: ManuallyDrop::new(inner),
                #[cfg(any(test, feature = "test-fixtures"))]
                fixture: None,
            })
        })
    }
    /// Open an existing authority without applying candidate migrations.
    /// Preflight and backup adapters must select this path for a live source.
    pub fn connect_existing(config: &DatabaseConfig) -> std::result::Result<Self, DatabaseError> {
        blocking(|| {
            Ok(Self {
                inner: ManuallyDrop::new(Persistence::connect(config)?),
                #[cfg(any(test, feature = "test-fixtures"))]
                fixture: None,
            })
        })
    }
    /// Compatibility convenience for a write-capable request. Pure reads must
    /// select `read`; background operations must select `maintenance`.
    pub fn call<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&mut Connection) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| self.inner.with_connection(request_lane(), work))
    }
    pub fn read<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&mut Connection) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| {
            if MAINTENANCE_CONTEXT.with(Cell::get) {
                self.inner.maintenance(|connection| {
                    connection.execute_batch("SET SESSION default_transaction_read_only=on")?;
                    work(connection)
                })
            } else {
                self.inner.read(work)
            }
        })
    }
    pub fn maintenance<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&mut Connection) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| self.inner.maintenance(work))
    }
    pub fn transaction<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| self.inner.transaction_on(request_lane(), work))
    }
    /// Optional metadata must never queue behind active domain mutations.
    pub(crate) fn try_transaction<T, F>(
        &self,
        work: F,
    ) -> std::result::Result<Option<T>, DatabaseError>
    where
        T: Send,
        F: FnOnce(&OptionalTransaction<'_, '_>) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| {
            self.inner
                .try_transaction_on(request_lane(), |transaction| {
                    let bounded = OptionalTransaction {
                        transaction,
                        deadline: Instant::now() + OPTIONAL_TRANSACTION_BUDGET,
                    };
                    let result = work(&bounded)?;
                    bounded.remaining()?;
                    Ok(result)
                })
        })
    }
    pub fn read_transaction<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| {
            let lane = if MAINTENANCE_CONTEXT.with(Cell::get) {
                Lane::Maintenance
            } else {
                Lane::Read
            };
            self.inner.transaction_on(lane, |tx| {
                tx.execute_batch("SET TRANSACTION READ ONLY")?;
                work(tx)
            })
        })
    }
    pub fn maintenance_transaction<T, F>(&self, work: F) -> std::result::Result<T, DatabaseError>
    where
        T: Send,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, DatabaseError> + Send,
    {
        blocking(|| self.inner.maintenance_transaction(work))
    }
    pub fn backup(&self, destination: PathBuf) -> std::result::Result<(), DatabaseError> {
        blocking(|| devcoordinator2_persistence::backup::dump(self.inner.config(), &destination))?;
        Ok(())
    }
    pub fn close(&self) -> std::result::Result<(), DatabaseError> {
        self.inner.close();
        Ok(())
    }
    pub fn persistence(&self) -> &Persistence {
        &self.inner
    }
    pub fn data_directory(&self) -> Option<&Path> {
        self.inner.config().data_directory.as_deref()
    }
    /// Tests may restart only their own disposable cluster.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn fixture(&self) -> Option<&FixtureDatabase> {
        self.fixture.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn asynchronous_caller_roundtrip() -> std::result::Result<(), DatabaseError> {
        let scratch = tempfile::tempdir().unwrap();
        let database = Database::open(scratch.path().join("authority-postgres-fixture"))?;
        database.transaction(|tx| {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES('async-fixture','kept')",
                [],
            )?;
            Ok(())
        })?;
        let value = database.read(|c| {
            Ok(c.query_row(
                "SELECT value FROM meta WHERE key='async-fixture'",
                [],
                |r| r.get::<_, String>(0),
            )?)
        })?;
        assert_eq!(value, "kept");
        maintenance_context(|| {
            database.read(|c| {
                let denied = c.execute(
                    "INSERT INTO meta(key,value) VALUES('forbidden','maintenance-read')",
                    [],
                );
                assert!(denied.is_err());
                Ok(())
            })
        })?;
        maintenance_context(|| {
            database.transaction(|tx| {
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES('maintenance-write','kept')",
                    [],
                )?;
                Ok(())
            })
        })?;
        assert_eq!(
            database.read(|c| Ok(c.query_row(
                "SELECT count(*) FROM meta WHERE key='maintenance-write'",
                [],
                |r| r.get::<_, i64>(0)
            )?))?,
            1
        );
        Ok(())
    }
    #[tokio::test(flavor = "current_thread")]
    async fn sync_postgres_boundary_works_inside_current_thread_runtime() {
        asynchronous_caller_roundtrip().unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_postgres_boundary_works_inside_multithread_runtime() {
        asynchronous_caller_roundtrip().unwrap();
    }
}
