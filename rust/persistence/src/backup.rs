//! Native private PostgreSQL archives; these are not SQLite database files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Connection, DatabaseConfig, Error, Lane, Persistence, Result};
pub fn archive_stamp(path: &Path) -> Result<crate::migration::FileStamp> {
    crate::migration::file_stamp(path)
}

pub fn dump(config: &DatabaseConfig, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Migration("backup requires a parent directory".into()))?;
    if destination.exists() {
        return Err(Error::Migration("backup destination already exists".into()));
    }
    let temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| Error::Migration(e.to_string()))?;
    let file = temporary.as_file();
    let mut command = command(config, "pg_dump")?;
    let status = command
        .args(["--format=custom", "--no-owner", "--no-acl"])
        .stdout(Stdio::from(
            file.try_clone()
                .map_err(|e| Error::Migration(e.to_string()))?,
        ))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Migration(e.to_string()))?;
    let status = wait_offline(status)?;
    if !status.success() {
        return Err(Error::Migration(
            "pg_dump failed; backup was not published".into(),
        ));
    }
    file.sync_all()
        .map_err(|e| Error::Migration(e.to_string()))?;
    temporary.persist_noclobber(destination).map_err(|_| {
        Error::Migration("backup destination already exists; refusing overwrite".into())
    })?;
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| Error::Migration(e.to_string()))?;
    Ok(())
}
pub fn restore(config: &DatabaseConfig, source: &Path) -> Result<()> {
    let stamp = crate::migration::file_stamp(source)?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|e| Error::Migration(e.to_string()))?;
    let mut magic = [0; 5];
    input
        .read_exact(&mut magic)
        .map_err(|e| Error::Migration(e.to_string()))?;
    if &magic != b"PGDMP" {
        return Err(Error::Migration(
            "snapshot is not a PostgreSQL custom archive".into(),
        ));
    }
    use std::io::{Seek, SeekFrom};
    input
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::Migration(e.to_string()))?;
    let mut command = command(config, "pg_restore")?;
    let status = command
        .args([
            "--no-owner",
            "--no-acl",
            "--exit-on-error",
            "--single-transaction",
            "--dbname",
        ])
        .arg(config.parsed()?.get_dbname().unwrap())
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Migration(e.to_string()))?;
    let status = wait_offline(status)?;
    if !status.success() {
        return Err(Error::Migration(
            "pg_restore failed; transaction rolled back".into(),
        ));
    }
    if crate::migration::file_stamp(source)? != stamp {
        return Err(Error::Migration("snapshot changed during restore".into()));
    }
    Ok(())
}
fn wait_offline(mut process: std::process::Child) -> Result<std::process::ExitStatus> {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = process
            .try_wait()
            .map_err(|e| Error::Migration(e.to_string()))?
        {
            return Ok(status);
        }
        if started.elapsed() >= std::time::Duration::from_secs(30 * 60)
            || crate::migration::check_offline_budget().is_err()
        {
            let _ = process.kill();
            let _ = process.wait();
            return Err(Error::Migration(
                "offline PostgreSQL archive operation exceeded its 30 minute deadline".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

struct OwnedSnapshot {
    authority: DatabaseConfig,
    name: String,
    removed: std::cell::Cell<bool>,
}

// Fixture-only stage diagnostics retain enough typed information to locate a
// timeout without exposing SQL, connection configuration, paths or records.
fn snapshot_stage<T>(stage: &'static str, result: Result<T>) -> Result<T> {
    #[cfg(feature = "test-fixtures")]
    if let Err(error) = &result {
        let category = match error {
            Error::Postgres(_) => "postgres",
            Error::Pool(_) => "pool",
            Error::Configuration(_) => "configuration",
            Error::Migration(_) => "migration",
            Error::SnapshotCleanup { .. } => "snapshot_cleanup",
            _ => "other",
        };
        let sqlstate = match error {
            Error::Postgres(error) => error.code().map(|code| code.code()),
            _ => None,
        };
        eprintln!(
            "native_snapshot_stage {}",
            serde_json::json!({"stage":stage,"category":category,"timeout":error.is_timeout(),"sqlstate":sqlstate})
        );
    }
    #[cfg(not(feature = "test-fixtures"))]
    let _ = stage;
    result
}

impl OwnedSnapshot {
    fn create(authority: &DatabaseConfig) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::InvalidQuery)?
            .as_nanos();
        let name = format!(
            "dc2_snapshot_{}_{}_{}",
            std::process::id(),
            nonce,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut client = snapshot_stage(
            "create_connect",
            authority
                .parsed()?
                .connect(postgres::NoTls)
                .map_err(Error::from),
        )?;
        snapshot_stage(
            "create_database",
            client
                .batch_execute(&format!(
                    "CREATE DATABASE \"{name}\" TEMPLATE template0 ENCODING 'UTF8'"
                ))
                .map_err(Error::from),
        )?;
        Ok(Self {
            authority: authority.clone(),
            name,
            removed: std::cell::Cell::new(false),
        })
    }
    fn remove(&self) -> Result<()> {
        if self.removed.get() {
            return Ok(());
        }
        let mut client = snapshot_stage(
            "cleanup_connect",
            self.authority
                .parsed()?
                .connect(postgres::NoTls)
                .map_err(Error::from),
        )?;
        snapshot_stage(
            "cleanup_settings",
            client
                .batch_execute("SET statement_timeout='15s'; SET lock_timeout='5s'")
                .map_err(Error::from),
        )?;
        // All clients owned by the inspection are already closed. Ordinary
        // DROP handles conflicting autovacuum workers internally; FORCE would
        // require signalling privileges that the authority role does not have.
        let removed = client.batch_execute(&format!("DROP DATABASE \"{}\"", self.name));
        snapshot_stage("cleanup_drop", removed.map_err(Error::from))?;
        self.removed.set(true);
        Ok(())
    }
}
impl Drop for OwnedSnapshot {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Restore one verified archive into a uniquely owned scratch database. The
/// callback receives only a read-only lane and all paths clean up this exact
/// database, including errors and unwinding. No other database is enumerated.
pub fn with_snapshot<T, E, F>(
    authority: &DatabaseConfig,
    archive: &Path,
    work: F,
) -> std::result::Result<T, E>
where
    E: From<Error> + std::fmt::Display,
    F: FnOnce(&Connection) -> std::result::Result<T, E>,
{
    let before =
        snapshot_stage("archive_stamp", crate::migration::file_stamp(archive)).map_err(E::from)?;
    let owned = OwnedSnapshot::create(authority).map_err(E::from)?;
    let config = authority.for_database(&owned.name);
    // Keep every owned client inside this scope so cleanup never races their
    // shutdown, including on restore, acquisition, callback or stamp failure.
    let result = (|| {
        snapshot_stage("restore", restore(&config, archive)).map_err(E::from)?;
        let database =
            snapshot_stage("snapshot_pool", Persistence::connect(&config)).map_err(E::from)?;
        let connection = snapshot_stage("snapshot_read_acquire", database.acquire(Lane::Read))
            .map_err(E::from)?;
        let result = work(&connection);
        let unchanged = snapshot_stage("archive_recheck", crate::migration::file_stamp(archive))
            .map_err(E::from)?
            == before;
        if !unchanged {
            return Err(E::from(Error::Migration(
                "snapshot changed during inspection".into(),
            )));
        }
        result
    })();
    match (result, owned.remove()) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(E::from(cleanup)),
        (Err(primary), Err(cleanup)) => Err(E::from(Error::SnapshotCleanup {
            primary: primary.to_string(),
            cleanup: Box::new(cleanup),
        })),
    }
}
fn command(config: &DatabaseConfig, name: &str) -> Result<Command> {
    let parsed = config.parsed()?;
    let mut candidates = Vec::new();
    if let Some(bin) = std::env::var_os("DEVCOORDINATOR2_TEST_POSTGRES_BIN") {
        candidates.push(PathBuf::from(bin).join(name));
    }
    if let Ok(versions) = fs::read_dir("/usr/lib/postgresql") {
        let mut paths = versions
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("bin").join(name))
            .collect::<Vec<_>>();
        paths.sort();
        paths.reverse();
        candidates.extend(paths);
    }
    let binary = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| Error::Configuration(format!("installed {name} is required")))?;
    let mut command = Command::new(binary);
    for name in [
        "PGHOSTADDR",
        "PGSERVICE",
        "PGSERVICEFILE",
        "PGOPTIONS",
        "PGDATABASE",
        "PGUSER",
        "PGPASSWORD",
        "PGPORT",
        "PGHOST",
    ] {
        command.env_remove(name);
    }
    if let postgres::config::Host::Unix(path) = &parsed.get_hosts()[0] {
        command.env("PGHOST", path);
    }
    if let Some(user) = parsed.get_user() {
        command.env("PGUSER", user);
    }
    if let Some(password) = parsed.get_password() {
        use std::os::unix::ffi::OsStrExt;
        command.env("PGPASSWORD", std::ffi::OsStr::from_bytes(password));
    }
    command
        .env("PGDATABASE", parsed.get_dbname().unwrap())
        .env(
            "PGPORT",
            parsed
                .get_ports()
                .first()
                .copied()
                .unwrap_or(5432)
                .to_string(),
        )
        .env(
            "PGCONNECT_TIMEOUT",
            config.connection_timeout.as_secs().max(1).to_string(),
        )
        .stdin(Stdio::null());
    Ok(command)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableFingerprint {
    pub table: String,
    pub columns: Vec<(String, String, String)>,
    pub rows: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SequenceFingerprint {
    pub sequence: String,
    pub value: i64,
    pub called: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveVerification {
    pub verified: bool,
    pub archive_sha256: String,
    pub archive_bytes: u64,
    pub tables: Vec<TableFingerprint>,
    pub sequences: Vec<SequenceFingerprint>,
    pub isolated_restore_removed: bool,
    pub matched_frozen_authority: bool,
}

const MAX_COMPARISON_CHANGES: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableComparisonChange {
    pub table: String,
    pub source_present: bool,
    pub restored_present: bool,
    pub source_rows: Option<u64>,
    pub restored_rows: Option<u64>,
    pub columns_equal: bool,
    pub rows_equal: bool,
    pub hash_equal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SequenceComparisonChange {
    pub sequence: String,
    pub source_present: bool,
    pub restored_present: bool,
    pub value_equal: bool,
    pub called_equal: bool,
}

/// Bounded private mismatch metadata. Never includes row values or connection data.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveComparisonChanges {
    pub source_table_count: usize,
    pub restored_table_count: usize,
    pub table_order_equal: bool,
    pub changed_table_count: usize,
    pub tables: Vec<TableComparisonChange>,
    pub source_sequence_count: usize,
    pub restored_sequence_count: usize,
    pub sequence_order_equal: bool,
    pub changed_sequence_count: usize,
    pub sequences: Vec<SequenceComparisonChange>,
    pub truncated: bool,
}

/// Diagnostic view of the same strict comparison used by `verify_archive`.
/// A mismatch remains `verified: false` and is never acceptance evidence.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveComparison {
    #[serde(flatten)]
    pub verification: ArchiveVerification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ArchiveComparisonChanges>,
}

fn comparison_changes(
    source_tables: &[TableFingerprint],
    restored_tables: &[TableFingerprint],
    source_sequences: &[SequenceFingerprint],
    restored_sequences: &[SequenceFingerprint],
) -> ArchiveComparisonChanges {
    let source = source_tables
        .iter()
        .map(|row| (row.table.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let restored = restored_tables
        .iter()
        .map(|row| (row.table.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut tables = Vec::new();
    for name in source
        .keys()
        .chain(restored.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let a = source.get(name).copied();
        let b = restored.get(name).copied();
        if a != b {
            tables.push(TableComparisonChange {
                table: name.to_owned(),
                source_present: a.is_some(),
                restored_present: b.is_some(),
                source_rows: a.map(|row| row.rows),
                restored_rows: b.map(|row| row.rows),
                columns_equal: a.zip(b).is_some_and(|(a, b)| a.columns == b.columns),
                rows_equal: a.zip(b).is_some_and(|(a, b)| a.rows == b.rows),
                hash_equal: a.zip(b).is_some_and(|(a, b)| a.sha256 == b.sha256),
            });
        }
    }
    let source = source_sequences
        .iter()
        .map(|row| (row.sequence.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let restored = restored_sequences
        .iter()
        .map(|row| (row.sequence.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut sequences = Vec::new();
    for name in source
        .keys()
        .chain(restored.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let a = source.get(name).copied();
        let b = restored.get(name).copied();
        if a != b {
            sequences.push(SequenceComparisonChange {
                sequence: name.to_owned(),
                source_present: a.is_some(),
                restored_present: b.is_some(),
                value_equal: a.zip(b).is_some_and(|(a, b)| a.value == b.value),
                called_equal: a.zip(b).is_some_and(|(a, b)| a.called == b.called),
            });
        }
    }
    let changed_table_count = tables.len();
    let changed_sequence_count = sequences.len();
    tables.truncate(MAX_COMPARISON_CHANGES);
    sequences.truncate(MAX_COMPARISON_CHANGES);
    ArchiveComparisonChanges {
        source_table_count: source_tables.len(),
        restored_table_count: restored_tables.len(),
        table_order_equal: source_tables
            .iter()
            .map(|row| &row.table)
            .eq(restored_tables.iter().map(|row| &row.table)),
        changed_table_count,
        tables,
        source_sequence_count: source_sequences.len(),
        restored_sequence_count: restored_sequences.len(),
        sequence_order_equal: source_sequences
            .iter()
            .map(|row| &row.sequence)
            .eq(restored_sequences.iter().map(|row| &row.sequence)),
        changed_sequence_count,
        sequences,
        truncated: changed_table_count > MAX_COMPARISON_CHANGES
            || changed_sequence_count > MAX_COMPARISON_CHANGES,
    }
}

/// Verify a native archive by restoring it and comparing every ordinary table
/// and sequence with the frozen authority. Only fingerprints leave this API.
pub fn verify_archive(config: &DatabaseConfig, archive: &Path) -> Result<ArchiveVerification> {
    let report = compare_archive(config, archive)?;
    if !report.verification.verified {
        return Err(Error::Migration(
            "restored archive differs from frozen authority data or identities".into(),
        ));
    }
    Ok(report.verification)
}

/// Retain content-free differences after restoring the exact archive into its
/// owned scratch database. Comparison, deadlines and cleanup remain strict.
pub fn compare_archive(config: &DatabaseConfig, archive: &Path) -> Result<ArchiveComparison> {
    let _budget = crate::migration::OfflineBudget::enter();
    let stamp = crate::migration::file_stamp(archive)?;
    let database = Persistence::connect(config)?;
    let (tables,sequences)=database.maintenance_transaction::<_,Error,_>(|tx|{
        tx.execute_batch("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout='30min'")?;
        Ok((table_fingerprints(tx)?,sequence_fingerprints(tx)?))
    })?;
    let (restored_tables, restored_sequences) =
        with_snapshot::<_, Error, _>(config, archive, |connection| {
            // This is an isolated restore; the read-only session gets the finite
            // offline verification allowance rather than request deadlines.
            connection.execute_batch("SET SESSION statement_timeout='30min'")?;
            Ok((
                table_fingerprints(connection)?,
                sequence_fingerprints(connection)?,
            ))
        })?;
    if crate::migration::file_stamp(archive)? != stamp {
        return Err(Error::Migration(
            "archive changed during verification".into(),
        ));
    }
    let verified = restored_tables == tables && restored_sequences == sequences;
    let comparison = (!verified)
        .then(|| comparison_changes(&tables, &restored_tables, &sequences, &restored_sequences));
    Ok(ArchiveComparison {
        verification: ArchiveVerification {
            verified,
            archive_sha256: stamp.sha256,
            archive_bytes: stamp.bytes,
            tables,
            sequences,
            isolated_restore_removed: true,
            matched_frozen_authority: verified,
        },
        comparison,
    })
}
fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn table_fingerprints(connection: &Connection) -> Result<Vec<TableFingerprint>> {
    use postgres::fallible_iterator::FallibleIterator;
    use sha2::{Digest, Sha256};
    let tables=connection.prepare("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') AND NOT c.relispartition ORDER BY c.relname")?.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>>>()?;
    let mut reports = Vec::new();
    for table in tables {
        crate::migration::check_offline_budget()?;
        let columns=connection.prepare("SELECT column_name,udt_name,is_nullable FROM information_schema.columns WHERE table_schema='public' AND table_name=$1 ORDER BY ordinal_position")?.query_map([&table],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<Result<Vec<_>>>()?;
        let keys = if columns.iter().any(|(name, _, _)| name == "rowid") {
            vec!["rowid".to_owned()]
        } else {
            connection.prepare("SELECT a.attname FROM pg_index i CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum,position) JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.attnum WHERE i.indrelid=$1::text::regclass AND i.indisprimary ORDER BY k.position")?.query_map([format!("public.{}",identifier(&table))],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>>>()?
        };
        if keys.is_empty() {
            return Err(Error::Migration(format!(
                "native archive table {table} has no deterministic key"
            )));
        }
        let sql = format!(
            "SELECT to_jsonb(t)::text FROM public.{} t ORDER BY {}",
            identifier(&table),
            keys.iter()
                .map(|key| identifier(key))
                .collect::<Vec<_>>()
                .join(",")
        );
        let (rows, sha256) = connection.with_client(|client| {
            let mut rows = client.query_raw(
                &sql,
                std::iter::empty::<&(dyn postgres::types::ToSql + Sync)>(),
            )?;
            let mut digest = Sha256::new();
            let mut count = 0_u64;
            while let Some(row) = rows.next()? {
                if count.is_multiple_of(1024) {
                    crate::migration::check_offline_budget()?;
                }
                let value: String = row.try_get(0)?;
                digest.update((value.len() as u64).to_be_bytes());
                digest.update(value.as_bytes());
                count += 1;
            }
            Ok((count, crate::hex(&digest.finalize())))
        })?;
        reports.push(TableFingerprint {
            table,
            columns,
            rows,
            sha256,
        });
    }
    Ok(reports)
}
fn sequence_fingerprints(connection: &Connection) -> Result<Vec<SequenceFingerprint>> {
    let names=connection.prepare("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind='S' ORDER BY c.relname")?.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>>>()?;
    names
        .into_iter()
        .map(|sequence| {
            let (value, called) = connection.query_row(
                &format!(
                    "SELECT last_value,is_called FROM public.{}",
                    identifier(&sequence)
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(SequenceFingerprint {
                sequence,
                value,
                called,
            })
        })
        .collect()
}

/// Recoverability proof for a hash-bound archive after live admission. It
/// validates only the isolated restored snapshot, without comparing it with
/// later writes to the running authority database.
pub fn verify_recoverable_archive(
    config: &DatabaseConfig,
    archive: &Path,
) -> Result<ArchiveVerification> {
    let _budget = crate::migration::OfflineBudget::enter();
    let stamp = crate::migration::file_stamp(archive)?;
    let (tables, sequences) = with_snapshot::<_, Error, _>(config, archive, |connection| {
        connection.execute_batch("SET SESSION statement_timeout='30min'")?;
        let version: String = connection.query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )?;
        if version != crate::schema::DATABASE_SCHEMA_VERSION.to_string() {
            return Err(Error::Migration(
                "archive has an unsupported authority schema".into(),
            ));
        }
        let invalid:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM pg_constraint WHERE connamespace='public'::regnamespace AND contype='f' AND NOT convalidated)",[],|r|r.get(0))?;
        if invalid {
            return Err(Error::Migration(
                "restored archive contains unvalidated foreign keys".into(),
            ));
        }
        let tables = table_fingerprints(connection)?;
        let sequences = sequence_fingerprints(connection)?;
        validate_identity_heads(connection)?;
        Ok((tables, sequences))
    })?;
    if crate::migration::file_stamp(archive)? != stamp {
        return Err(Error::Migration(
            "archive changed during recoverability verification".into(),
        ));
    }
    Ok(ArchiveVerification {
        verified: true,
        archive_sha256: stamp.sha256,
        archive_bytes: stamp.bytes,
        tables,
        sequences,
        isolated_restore_removed: true,
        matched_frozen_authority: false,
    })
}
fn validate_identity_heads(connection: &Connection) -> Result<()> {
    let keys=connection.prepare("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' AND is_identity='YES' AND column_name NOT IN ('port','chat_id','singleton') AND table_name IN (SELECT relname FROM pg_class WHERE relnamespace='public'::regnamespace AND NOT relispartition) ORDER BY table_name,column_name")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>>>()?;
    for (table, column) in keys {
        let maximum: Option<i64> = connection.query_row(
            &format!(
                "SELECT max({}) FROM public.{}",
                identifier(&column),
                identifier(&table)
            ),
            [],
            |r| r.get(0),
        )?;
        let Some(maximum) = maximum else {
            continue;
        };
        let sequence: String = connection.query_row(
            "SELECT c.relname FROM pg_class c WHERE c.oid=pg_get_serial_sequence($1,$2)::regclass",
            crate::params![format!("public.{}", identifier(&table)), column],
            |r| r.get(0),
        )?;
        let (value, called): (i64, bool) = connection.query_row(
            &format!(
                "SELECT last_value,is_called FROM public.{}",
                identifier(&sequence)
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if value < maximum || value == maximum && !called {
            return Err(Error::Migration(
                "restored generated identity could reuse an existing row identifier".into(),
            ));
        }
    }
    Ok(())
}

/// Restore into a new database only. No existing user relation, routine, type
/// or non-system schema may be replaced by this operation.
pub fn restore_candidate(config: &DatabaseConfig, archive: &Path) -> Result<ArchiveVerification> {
    let _budget = crate::migration::OfflineBudget::enter();
    let stamp = crate::migration::file_stamp(archive)?;
    let database = Persistence::connect(config)?;
    let occupied=database.read::<_,Error,_>(|c|c.query_row("SELECT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public') OR EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public') OR EXISTS(SELECT 1 FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='public') OR EXISTS(SELECT 1 FROM pg_namespace WHERE nspname NOT IN ('public','pg_catalog','information_schema') AND nspname NOT LIKE 'pg_toast%' AND nspname NOT LIKE 'pg_temp_%')",[],|r|r.get::<_,bool>(0)))?;
    if occupied {
        return Err(Error::Migration(
            "candidate PostgreSQL database is not pristine; refusing overwrite".into(),
        ));
    }
    drop(database);
    restore(config, archive)?;
    let database = Persistence::connect(config)?;
    let (tables,sequences)=database.maintenance_transaction::<_,Error,_>(|tx|{
        tx.execute_batch("SET TRANSACTION READ ONLY; SET LOCAL statement_timeout='30min'; SET LOCAL idle_in_transaction_session_timeout='30min'")?;
        validate_identity_heads(tx)?;
        Ok((table_fingerprints(tx)?,sequence_fingerprints(tx)?))
    })?;
    if crate::migration::file_stamp(archive)? != stamp {
        return Err(Error::Migration(
            "archive changed during candidate restoration".into(),
        ));
    }
    Ok(ArchiveVerification {
        verified: true,
        archive_sha256: stamp.sha256,
        archive_bytes: stamp.bytes,
        tables,
        sequences,
        isolated_restore_removed: false,
        matched_frozen_authority: false,
    })
}
