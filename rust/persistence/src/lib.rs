//! PostgreSQL is the only writable Coordinator authority database.
//!
//! Request reads, request writes, and maintenance use independent bounded pools.
//! SQLite is linked only by the explicit read-only migration importer.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use postgres::{Config as PostgresConfig, NoTls, config::Host};
use r2d2::{Pool, PooledConnection};
use r2d2_postgres::PostgresConnectionManager;
use thiserror::Error;

pub mod backup;
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixture;
pub mod maintenance;
pub mod migration;
mod query;
pub mod schema;

pub use query::{
    Connection, FromValue, OptionalExtension, Params, Row, RowIndex, Rows, Statement, ToValue,
    Transaction, Value, params_from_iter, types,
};

pub type Manager = PostgresConnectionManager<NoTls>;
pub type ConnectionPool = Pool<Manager>;
pub type PooledClient = PooledConnection<Manager>;
pub type PersistenceError = Error;
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSizes {
    pub reads: u32,
    pub writes: u32,
    pub maintenance: u32,
}
impl Default for PoolSizes {
    fn default() -> Self {
        Self {
            reads: 8,
            writes: 4,
            maintenance: 2,
        }
    }
}
impl PoolSizes {
    fn validate(self) -> Result<Self> {
        if [self.reads, self.writes, self.maintenance]
            .iter()
            .any(|n| *n == 0 || *n > 128)
        {
            return Err(Error::Configuration(
                "pool sizes must be between 1 and 128 per lane".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone)]
pub struct DatabaseConfig {
    /// A libpq-style connection string with an absolute Unix socket directory.
    pub url: String,
    pub pool_sizes: PoolSizes,
    pub connection_timeout: Duration,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
    pub idle_transaction_timeout: Duration,
    /// Private physical authority root retained for destructive-path guards.
    pub data_directory: Option<std::path::PathBuf>,
    database_override: Option<String>,
}
impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("url", &"<private Unix-socket configuration>")
            .field("pool_sizes", &self.pool_sizes)
            .field("connection_timeout", &self.connection_timeout)
            .field("statement_timeout", &self.statement_timeout)
            .field("lock_timeout", &self.lock_timeout)
            .finish_non_exhaustive()
    }
}
impl DatabaseConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            pool_sizes: PoolSizes::default(),
            connection_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(3),
            idle_transaction_timeout: Duration::from_secs(15),
            data_directory: None,
            database_override: None,
        }
    }
    pub fn for_database(&self, name: &str) -> Self {
        let mut config = self.clone();
        config.database_override = Some(name.to_owned());
        config
    }
    pub fn from_env() -> Result<Self> {
        let path=std::env::var_os("DEVCOORDINATOR2_DATABASE_CONFIG").ok_or_else(||Error::Configuration("DEVCOORDINATOR2_DATABASE_CONFIG private file is required; DSN environment and SQLite authority fallbacks are unsupported".into()))?;
        Self::from_file(std::path::Path::new(&path))
    }
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Private {
            schema: u8,
            connection: String,
            data_directory: Option<std::path::PathBuf>,
            pools: Option<PoolSizes>,
            timeouts_ms: Option<PrivateTimeouts>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PrivateTimeouts {
            connection: Option<u64>,
            statement: Option<u64>,
            lock: Option<u64>,
            idle_transaction: Option<u64>,
        }
        let invalid = || {
            Error::Configuration("private PostgreSQL configuration must be an absolute mode-0600 regular file owned by the current identity".into())
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| invalid())?;
        let metadata = file.metadata().map_err(|_| invalid())?;
        if !path.is_absolute()
            || !metadata.is_file()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() > 64 * 1024
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() > 64 * 1024 {
            return Err(invalid());
        }
        let private: Private = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if private.schema != 1 || private.connection.is_empty() {
            return Err(invalid());
        }
        let mut config = Self::new(private.connection);
        config.data_directory = private.data_directory;
        config.normalize_data_directory()?;
        if let Some(pools) = private.pools {
            config.pool_sizes = pools;
        }
        if let Some(timeouts) = private.timeouts_ms {
            if let Some(ms) = timeouts.connection {
                config.connection_timeout = Duration::from_millis(ms);
            }
            if let Some(ms) = timeouts.statement {
                config.statement_timeout = Duration::from_millis(ms);
            }
            if let Some(ms) = timeouts.lock {
                config.lock_timeout = Duration::from_millis(ms);
            }
            if let Some(ms) = timeouts.idle_transaction {
                config.idle_transaction_timeout = Duration::from_millis(ms);
            }
        }
        config.parsed()?;
        Ok(config)
    }
    fn normalize_data_directory(&mut self) -> Result<()> {
        if let Some(path) = &self.data_directory {
            let invalid = || {
                Error::Configuration("private PostgreSQL data_directory must identify an existing absolute directory without parent traversal".into())
            };
            if !path.is_absolute()
                || path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
            {
                return Err(invalid());
            }
            let physical = std::fs::canonicalize(path).map_err(|_| invalid())?;
            if !physical.is_dir() {
                return Err(invalid());
            }
            self.data_directory = Some(physical);
        }
        Ok(())
    }
    fn parsed(&self) -> Result<PostgresConfig> {
        self.pool_sizes.validate()?;
        if [
            self.connection_timeout,
            self.statement_timeout,
            self.lock_timeout,
            self.idle_transaction_timeout,
        ]
        .iter()
        .any(Duration::is_zero)
        {
            return Err(Error::Configuration(
                "database timeouts must be positive".into(),
            ));
        }
        let mut parsed = self
            .url
            .parse::<PostgresConfig>()
            .map_err(|_| Error::Configuration("invalid PostgreSQL connection string".into()))?;
        if parsed.get_hosts().is_empty()
            || !parsed.get_hostaddrs().is_empty()
            || parsed
                .get_hosts()
                .iter()
                .any(|host| !matches!(host,Host::Unix(path) if path.is_absolute()))
        {
            return Err(Error::Configuration("dedicated PostgreSQL authority requires an absolute Unix socket directory, without TCP hosts".into()));
        }
        if parsed.get_dbname().is_none() {
            return Err(Error::Configuration("database name is required".into()));
        }
        parsed.connect_timeout(self.connection_timeout);
        parsed.application_name("devcoordinator2-v3");
        if let Some(name) = &self.database_override {
            parsed.dbname(name);
        }
        Ok(parsed)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("PostgreSQL configuration is invalid: {0}")]
    Configuration(String),
    #[error("PostgreSQL pool is unavailable or saturated")]
    Pool(#[from] r2d2::Error),
    #[error("PostgreSQL operation failed: {0}")]
    Postgres(#[from] postgres::Error),
    #[error("PostgreSQL migration failed: {0}")]
    Migration(String),
    #[error(
        "snapshot operation failed ({primary}); owned snapshot cleanup also failed ({cleanup})"
    )]
    SnapshotCleanup {
        primary: String,
        #[source]
        cleanup: Box<Error>,
    },
    #[error("database fixture failed: {0}")]
    Fixture(String),
    #[error("database is closed")]
    Closed,
    #[error("query returned no rows")]
    QueryReturnedNoRows,
    #[error("invalid query or transaction state")]
    InvalidQuery,
    #[error("column {0} is outside the result")]
    InvalidColumnIndex(usize),
    #[error("unknown result column {0}")]
    InvalidColumnName(String),
    #[error("integer in column {0} is outside the requested Rust type")]
    IntegralValueOutOfRange(usize, i64),
    #[error("cannot decode column {0} ({1:?}): {2}")]
    FromSqlConversionFailure(
        usize,
        types::Type,
        #[source] Box<dyn std::error::Error + Send + Sync>,
    ),
    #[error("database callback failed: {0}")]
    UserFunctionError(#[source] Box<dyn std::error::Error + Send + Sync>),
}
impl Error {
    pub fn decode(index: usize, message: impl Into<String>) -> Self {
        Self::FromSqlConversionFailure(index, types::Type::Text, message.into().into())
    }
    pub fn is_unique_violation(&self) -> bool {
        matches!(self,Self::Postgres(e) if e.code()==Some(&postgres::error::SqlState::UNIQUE_VIOLATION))
    }
    pub fn is_constraint_violation(&self) -> bool {
        matches!(self,Self::Postgres(e) if e.code().is_some_and(|code|code.code().starts_with("23")))
    }
    pub fn is_timeout(&self) -> bool {
        matches!(self,Self::Postgres(e) if matches!(e.code(),Some(code) if code==&postgres::error::SqlState::QUERY_CANCELED || code==&postgres::error::SqlState::LOCK_NOT_AVAILABLE))
            || matches!(self,Self::SnapshotCleanup {cleanup,..} if cleanup.is_timeout())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lane {
    Read,
    Write,
    Maintenance,
}

struct Pools {
    reads: ConnectionPool,
    writes: ConnectionPool,
    maintenance: ConnectionPool,
    closed: AtomicBool,
    config: DatabaseConfig,
}
#[derive(Clone)]
pub struct Persistence {
    pools: Arc<Pools>,
}

impl Persistence {
    pub fn connect(config: &DatabaseConfig) -> Result<Self> {
        let mut config = config.clone();
        config.normalize_data_directory()?;
        let parsed = config.parsed()?;
        let sizes = config.pool_sizes.validate()?;
        let reads = build_pool(&parsed, sizes.reads, &config, Lane::Read)?;
        let writes = build_pool(&parsed, sizes.writes, &config, Lane::Write)?;
        let maintenance = build_pool(&parsed, sizes.maintenance, &config, Lane::Maintenance)?;
        Ok(Self {
            pools: Arc::new(Pools {
                reads,
                writes,
                maintenance,
                closed: AtomicBool::new(false),
                config,
            }),
        })
    }
    pub fn pool(&self, lane: Lane) -> &ConnectionPool {
        match lane {
            Lane::Read => &self.pools.reads,
            Lane::Write => &self.pools.writes,
            Lane::Maintenance => &self.pools.maintenance,
        }
    }
    pub fn acquire(&self, lane: Lane) -> Result<Connection> {
        if self.pools.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let mut client = self
            .pool(lane)
            .get_timeout(self.pools.config.connection_timeout)?;
        // SESSION settings apply to implicit and explicit transactions. SET
        // LOCAL before BEGIN would silently do nothing.
        client.batch_execute(&session_settings(&self.pools.config, lane))?;
        Ok(Connection::new(client))
    }
    /// Admit optional work only when this lane already has a connection.
    /// Saturation does not queue a request or borrow another lane's capacity.
    pub fn try_acquire(&self, lane: Lane) -> Result<Option<Connection>> {
        if self.pools.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let Some(mut client) = self.pool(lane).try_get() else {
            return Ok(None);
        };
        client.batch_execute(&session_settings(&self.pools.config, lane))?;
        Ok(Some(Connection::new(client)))
    }
    pub fn read<T, E, F>(&self, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&mut Connection) -> std::result::Result<T, E>,
    {
        self.with_connection(Lane::Read, work)
    }
    pub fn write<T, E, F>(&self, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&mut Connection) -> std::result::Result<T, E>,
    {
        self.with_connection(Lane::Write, work)
    }
    pub fn maintenance<T, E, F>(&self, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&mut Connection) -> std::result::Result<T, E>,
    {
        self.with_connection(Lane::Maintenance, work)
    }
    pub fn with_connection<T, E, F>(&self, lane: Lane, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&mut Connection) -> std::result::Result<T, E>,
    {
        let mut connection = self.acquire(lane).map_err(E::from)?;
        work(&mut connection)
    }
    pub fn transaction<T, E, F>(&self, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, E>,
    {
        self.transaction_on(Lane::Write, work)
    }
    pub fn maintenance_transaction<T, E, F>(&self, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, E>,
    {
        self.transaction_on(Lane::Maintenance, work)
    }
    pub fn transaction_on<T, E, F>(&self, lane: Lane, work: F) -> std::result::Result<T, E>
    where
        E: From<Error>,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, E>,
    {
        let connection = self.acquire(lane).map_err(E::from)?;
        let tx = connection.unchecked_transaction().map_err(E::from)?;
        let value = work(&tx)?;
        tx.commit().map_err(E::from)?;
        Ok(value)
    }
    pub fn try_transaction_on<T, E, F>(
        &self,
        lane: Lane,
        work: F,
    ) -> std::result::Result<Option<T>, E>
    where
        E: From<Error>,
        F: FnOnce(&Transaction<'_>) -> std::result::Result<T, E>,
    {
        let Some(connection) = self.try_acquire(lane).map_err(E::from)? else {
            return Ok(None);
        };
        let tx = connection.unchecked_transaction().map_err(E::from)?;
        let value = work(&tx)?;
        tx.commit().map_err(E::from)?;
        Ok(Some(value))
    }
    pub fn close(&self) {
        self.pools.closed.store(true, Ordering::Release);
    }
    pub fn migrate(&self) -> Result<()> {
        schema::migrate(self)
    }
    pub fn config(&self) -> &DatabaseConfig {
        &self.pools.config
    }
}

fn session_settings(config: &DatabaseConfig, lane: Lane) -> String {
    format!(
        "ROLLBACK; SET SESSION statement_timeout = {}; SET SESSION lock_timeout = {}; SET SESSION idle_in_transaction_session_timeout = {}; SET SESSION default_transaction_read_only = {}; SET SESSION jit = off; SET SESSION TIME ZONE 'UTC';",
        config.statement_timeout.as_millis().min(i32::MAX as u128),
        config.lock_timeout.as_millis().min(i32::MAX as u128),
        config
            .idle_transaction_timeout
            .as_millis()
            .min(i32::MAX as u128),
        if lane == Lane::Read { "on" } else { "off" }
    )
}
fn build_pool(
    parsed: &PostgresConfig,
    size: u32,
    config: &DatabaseConfig,
    lane: Lane,
) -> Result<ConnectionPool> {
    let mut parsed = parsed.clone();
    parsed.options(&format!("-c statement_timeout={} -c lock_timeout={} -c idle_in_transaction_session_timeout={} -c default_transaction_read_only={} -c jit=off",config.statement_timeout.as_millis().min(i32::MAX as u128),config.lock_timeout.as_millis().min(i32::MAX as u128),config.idle_transaction_timeout.as_millis().min(i32::MAX as u128),if lane==Lane::Read{"on"}else{"off"}));
    let manager = PostgresConnectionManager::new(parsed, NoTls);
    Ok(Pool::builder()
        .max_size(size)
        .min_idle(Some(0))
        .connection_timeout(config.connection_timeout)
        .test_on_check_out(true)
        .build(manager)?)
}
