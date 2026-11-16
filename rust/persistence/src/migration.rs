//! Frozen, read-only SQLite -> PostgreSQL COPY migration with typed evidence.
//!
//! Inspection/copy never deletes or modifies the source. Cutover publication
//! and final source retirement require the operator's separate acceptance.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
thread_local! {static OFFLINE_DEADLINE:std::cell::Cell<Option<Instant>>=const {std::cell::Cell::new(None)};}
pub(crate) struct OfflineBudget(Option<Instant>);
impl OfflineBudget {
    pub(crate) fn enter() -> Self {
        Self(
            OFFLINE_DEADLINE
                .with(|deadline| deadline.replace(Some(Instant::now() + OFFLINE_TIMEOUT))),
        )
    }
}
impl Drop for OfflineBudget {
    fn drop(&mut self) {
        OFFLINE_DEADLINE.with(|deadline| deadline.set(self.0));
    }
}
pub(crate) fn check_offline_budget() -> Result<()> {
    if OFFLINE_DEADLINE.with(|deadline| deadline.get().is_some_and(|end| Instant::now() >= end)) {
        return Err(Error::Migration(
            "offline operation exceeded its 30 minute deadline; source retained".into(),
        ));
    }
    Ok(())
}

use postgres::binary_copy::BinaryCopyInWriter;
use postgres::types::{ToSql, Type};
use rusqlite::{Connection as Sqlite, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Connection, Error, Persistence, Result, Value, params};

pub type InspectionError = Error;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileStamp {
    pub sha256: String,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TableReport {
    pub table: String,
    pub columns: Vec<String>,
    pub rows: u64,
    pub checksum_sha256: String,
    pub blob_bytes: u64,
    pub has_rowid: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceReport {
    pub schema_version: Option<u32>,
    pub schema_metadata_sha256: String,
    pub file: FileStamp,
    pub tables: Vec<TableReport>,
    pub sequences: BTreeMap<String, i64>,
}
impl SourceReport {
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TableCheck {
    pub source: TableReport,
    pub target: TableReport,
    pub matched: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportReceipt {
    pub schema: u8,
    pub source: SourceReport,
    pub checks: Vec<TableCheck>,
    pub source_unchanged: bool,
    pub foreign_keys_valid: bool,
    pub identities_restored: bool,
    pub committed: bool,
    pub source_retained: bool,
    pub receipt_sha256: String,
}

struct FrozenSource {
    connection: Sqlite,
    path: PathBuf,
    stamp: FileStamp,
}
impl FrozenSource {
    fn open(path: &Path) -> Result<Self> {
        reject_sidecars(path)?;
        let stamp = file_stamp(path)?;
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().map_err(io_error)?.join(path)
        };
        let encoded = absolute
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
                    char::from(*byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>();
        // immutable=1 prevents SQLite creating WAL/shared-memory sidecars even
        // for a clean WAL-mode source. Offline freezing is checked above and
        // its full file identity/hash is checked again before commit.
        let connection = Sqlite::open_with_flags(
            format!("file:{encoded}?mode=ro&immutable=1"),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        )
        .map_err(sqlite_error)?;
        if let Some(deadline) = OFFLINE_DEADLINE.with(std::cell::Cell::get) {
            connection
                .progress_handler(10_000, Some(move || Instant::now() >= deadline))
                .map_err(sqlite_error)?;
        }
        connection
            .execute_batch("PRAGMA query_only=ON; BEGIN")
            .map_err(sqlite_error)?;
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(sqlite_error)?;
        if integrity != "ok" {
            return Err(Error::Migration("source integrity check failed".into()));
        }
        let mut check = connection
            .prepare("PRAGMA foreign_key_check")
            .map_err(sqlite_error)?;
        let mut violations = check.query([]).map_err(sqlite_error)?;
        if violations.next().map_err(sqlite_error)?.is_some() {
            return Err(Error::Migration("source has foreign-key violations".into()));
        }
        drop(violations);
        drop(check);
        Ok(Self {
            connection,
            path: path.to_owned(),
            stamp,
        })
    }
    fn unchanged(&self) -> Result<bool> {
        reject_sidecars(&self.path)?;
        Ok(self.stamp == file_stamp(&self.path)?)
    }
}
fn sqlite_error(_error: rusqlite::Error) -> Error {
    if let Err(error) = check_offline_budget() {
        return error;
    }
    Error::Migration("SQLite source could not be decoded; source remains intact".into())
}
fn io_error(error: std::io::Error) -> Error {
    Error::Migration(error.to_string())
}
fn reject_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match fs::symlink_metadata(PathBuf::from(name)) {
            Ok(metadata) if metadata.len() > 0 || !metadata.is_file() => {
                return Err(Error::Migration(
                    "source is not frozen: checkpoint and close SQLite before copying".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
        }
    }
    Ok(())
}
pub(crate) fn file_stamp(path: &Path) -> Result<FileStamp> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(Error::Migration("source must be a regular file".into()));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        check_offline_budget()?;
        let n = file.read(&mut buffer).map_err(io_error)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let after = file.metadata().map_err(io_error)?;
    if metadata.len() != after.len()
        || metadata.mtime() != after.mtime()
        || metadata.mtime_nsec() != after.mtime_nsec()
    {
        return Err(Error::Migration(
            "source changed while it was hashed".into(),
        ));
    }
    Ok(FileStamp {
        sha256: crate::hex(&digest.finalize()),
        bytes: metadata.len(),
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    })
}
#[derive(Clone)]
struct SourceTable {
    name: String,
    columns: Vec<String>,
    has_rowid: bool,
    parents: BTreeSet<String>,
}
fn tables(connection: &Sqlite) -> Result<Vec<SourceTable>> {
    // table_list supplies exact table/virtual/shadow classification. A normal
    // user table whose name contains `_fts` is preserved, not guessed away.
    let names = connection
        .prepare("PRAGMA table_list")
        .map_err(sqlite_error)?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(4)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut result = Vec::new();
    for (name, kind, without_rowid) in names {
        if kind != "table" || name.starts_with("sqlite_") {
            continue;
        }
        let columns = connection
            .prepare(&format!("PRAGMA table_xinfo({})", quoted(&name)))
            .map_err(sqlite_error)?
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(6)?)))
            .map_err(sqlite_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sqlite_error)?
            .into_iter()
            .filter(|(_, hidden)| *hidden == 0)
            .map(|(name, _)| name)
            .collect();
        let parents = connection
            .prepare(&format!("PRAGMA foreign_key_list({})", quoted(&name)))
            .map_err(sqlite_error)?
            .query_map([], |r| r.get::<_, String>(2))
            .map_err(sqlite_error)?
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .map_err(sqlite_error)?;
        result.push(SourceTable {
            name,
            columns,
            has_rowid: !without_rowid,
            parents,
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    // Stable FK order. Self/cyclic references are deferred in PostgreSQL; a
    // stable final suffix still preserves every row in one transaction.
    let all = result
        .iter()
        .map(|t| t.name.clone())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::new();
    let mut done = BTreeSet::new();
    while !result.is_empty() {
        let index = result
            .iter()
            .position(|t| {
                t.parents
                    .iter()
                    .all(|p| p == &t.name || done.contains(p) || !all.contains(p))
            })
            .unwrap_or(0);
        let table = result.remove(index);
        done.insert(table.name.clone());
        ordered.push(table);
    }
    Ok(ordered)
}
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn source_sql(table: &SourceTable) -> String {
    let cols = table
        .columns
        .iter()
        .map(|c| quoted(c))
        .collect::<Vec<_>>()
        .join(",");
    if table.has_rowid {
        format!(
            "SELECT rowid,{cols} FROM {} ORDER BY rowid",
            quoted(&table.name)
        )
    } else {
        format!("SELECT {cols} FROM {} ORDER BY {cols}", quoted(&table.name))
    }
}
fn is_json(table: &str, column: &str) -> bool {
    column.ends_with("_json") && !(table == "agent_hub_outbox" && column == "payload_json")
}
fn source_cell(value: ValueRef<'_>, json: bool) -> Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => Value::Integer(v),
        ValueRef::Real(v) => Value::Real(v),
        ValueRef::Blob(v) => Value::Blob(v.to_vec()),
        ValueRef::Text(v) if json => {
            let value = std::str::from_utf8(v)
                .map_err(|_| Error::Migration("source contains invalid serialized JSON".into()))?;
            serde_json::from_str::<Box<serde_json::value::RawValue>>(value)
                .map_err(|_| Error::Migration("source contains invalid serialized JSON".into()))?;
            Value::JsonText(value.to_owned())
        }
        ValueRef::Text(v) => Value::Text(
            std::str::from_utf8(v)
                .map_err(|_| Error::Migration("source contains invalid UTF-8 text".into()))?
                .to_owned(),
        ),
    })
}
fn digest_cell(digest: &mut Sha256, value: &Value, blob_bytes: &mut u64) -> Result<()> {
    check_offline_budget()?;
    let (tag, bytes): (u8, Vec<u8>) = match value {
        Value::Null => (0, Vec::new()),
        Value::Integer(n) => (1, n.to_be_bytes().to_vec()),
        Value::Unsigned(n) => (
            1,
            i64::try_from(*n)
                .map_err(|_| Error::InvalidQuery)?
                .to_be_bytes()
                .to_vec(),
        ),
        Value::Real(n) => (2, n.to_bits().to_be_bytes().to_vec()),
        Value::Text(s) => (3, s.as_bytes().to_vec()),
        Value::Blob(b) => {
            *blob_bytes = blob_bytes.saturating_add(b.len() as u64);
            (4, b.clone())
        }
        Value::Bool(b) => (1, i64::from(*b).to_be_bytes().to_vec()),
        Value::Json(j) => (
            5,
            canonical_json(
                &serde_json::to_string(j).map_err(|e| Error::Migration(e.to_string()))?,
            )?,
        ),
        Value::JsonText(j) => (5, canonical_json(j)?),
    };
    digest.update([tag]);
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    Ok(())
}
/// JSONB uses a canonical semantic representation, with sorted object keys;
/// raw whitespace is not meaningful. Opaque signed wire bytes remain TEXT.
fn canonical_json(value: &str) -> Result<Vec<u8>> {
    fn number(text: &str) -> Result<String> {
        let (negative, text) = text.strip_prefix('-').map_or((false, text), |v| (true, v));
        let (mantissa, exponent) = if let Some((m, e)) = text.split_once(['e', 'E']) {
            (
                m,
                e.parse::<i64>().map_err(|_| {
                    Error::Migration("JSON number exponent is outside PostgreSQL range".into())
                })?,
            )
        } else {
            (text, 0)
        };
        let fraction = mantissa
            .split_once('.')
            .map_or(0, |(_, fraction)| fraction.len());
        let mut power = exponent
            .checked_sub(fraction as i64)
            .ok_or(Error::InvalidQuery)?;
        let mut digits = mantissa
            .chars()
            .filter(|c| *c != '.')
            .collect::<String>()
            .trim_start_matches('0')
            .to_owned();
        if digits.is_empty() {
            return Ok("0".into());
        }
        while digits.ends_with('0') {
            digits.pop();
            power = power.checked_add(1).ok_or(Error::InvalidQuery)?;
        }
        Ok(format!(
            "{}{digits}e{power}",
            if negative { "-" } else { "" }
        ))
    }
    fn encode(value: &str, out: &mut Vec<u8>) -> Result<()> {
        fn bytes(out: &mut Vec<u8>, value: &[u8]) {
            out.extend_from_slice(&(value.len() as u64).to_be_bytes());
            out.extend_from_slice(value);
        }
        let value = value.trim();
        let invalid = || Error::Migration("source contains invalid serialized JSON".into());
        match value.as_bytes().first().copied() {
            Some(b'n') if value == "null" => out.push(0),
            Some(b't') if value == "true" => {
                out.push(1);
                out.push(1);
            }
            Some(b'f') if value == "false" => {
                out.push(1);
                out.push(0);
            }
            Some(b'"') => {
                let string: String = serde_json::from_str(value).map_err(|_| invalid())?;
                out.push(3);
                bytes(out, string.as_bytes());
            }
            Some(b'[') => {
                let values: Vec<Box<serde_json::value::RawValue>> =
                    serde_json::from_str(value).map_err(|_| invalid())?;
                out.push(4);
                out.extend_from_slice(&(values.len() as u64).to_be_bytes());
                for value in values {
                    encode(value.get(), out)?;
                }
            }
            Some(b'{') => {
                let values: BTreeMap<String, Box<serde_json::value::RawValue>> =
                    serde_json::from_str(value).map_err(|_| invalid())?;
                out.push(5);
                out.extend_from_slice(&(values.len() as u64).to_be_bytes());
                for (key, value) in values {
                    bytes(out, key.as_bytes());
                    encode(value.get(), out)?;
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                serde_json::from_str::<Box<serde_json::value::RawValue>>(value)
                    .map_err(|_| invalid())?;
                out.push(2);
                bytes(out, number(value)?.as_bytes());
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    let mut result = Vec::new();
    encode(value, &mut result)?;
    Ok(result)
}
fn inspect_table(source: &Sqlite, table: &SourceTable) -> Result<TableReport> {
    let mut query = source.prepare(&source_sql(table)).map_err(sqlite_error)?;
    let mut rows = query.query([]).map_err(sqlite_error)?;
    let mut digest = Sha256::new();
    let mut count = 0;
    let mut blob_bytes = 0;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        let offset = usize::from(table.has_rowid);
        if table.has_rowid {
            digest_cell(
                &mut digest,
                &source_cell(row.get_ref(0).map_err(sqlite_error)?, false)?,
                &mut blob_bytes,
            )?;
        }
        for (index, name) in table.columns.iter().enumerate() {
            digest_cell(
                &mut digest,
                &source_cell(
                    row.get_ref(index + offset).map_err(sqlite_error)?,
                    is_json(&table.name, name),
                )?,
                &mut blob_bytes,
            )?;
        }
        digest.update([0xff]);
        count += 1;
    }
    Ok(TableReport {
        table: table.name.clone(),
        columns: table.columns.clone(),
        rows: count,
        checksum_sha256: crate::hex(&digest.finalize()),
        blob_bytes,
        has_rowid: table.has_rowid,
    })
}
fn report(source: &FrozenSource, tables: &[SourceTable]) -> Result<SourceReport> {
    let schema_version = source
        .connection
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());
    let has_sequences = source
        .connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sqlite_sequence'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    let sequences = if has_sequences {
        source
            .connection
            .prepare("SELECT name,seq FROM sqlite_sequence ORDER BY name")
            .map_err(sqlite_error)?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(sqlite_error)?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(sqlite_error)?
    } else {
        BTreeMap::new()
    };
    let mut schema_digest = Sha256::new();
    let mut metadata = source
        .connection
        .prepare("SELECT type,name,tbl_name,coalesce(sql,'') FROM sqlite_master ORDER BY type,name")
        .map_err(sqlite_error)?;
    let mut metadata_rows = metadata.query([]).map_err(sqlite_error)?;
    while let Some(row) = metadata_rows.next().map_err(sqlite_error)? {
        for i in 0..4 {
            let value: String = row.get(i).map_err(sqlite_error)?;
            schema_digest.update((value.len() as u64).to_be_bytes());
            schema_digest.update(value.as_bytes());
        }
    }
    Ok(SourceReport {
        schema_version,
        schema_metadata_sha256: crate::hex(&schema_digest.finalize()),
        file: source.stamp.clone(),
        tables: tables
            .iter()
            .map(|t| inspect_table(&source.connection, t))
            .collect::<Result<Vec<_>>>()?,
        sequences,
    })
}
pub fn inspect_sqlite(path: &Path) -> Result<SourceReport> {
    let _budget = OfflineBudget::enter();
    let source = FrozenSource::open(path)?;
    let tables = tables(&source.connection)?;
    let report = report(&source, &tables)?;
    if !source.unchanged()? {
        return Err(Error::Migration("source changed during inspection".into()));
    }
    Ok(report)
}

fn target_columns(connection: &Connection, table: &SourceTable) -> Result<Vec<(String, Type)>> {
    let cols = table
        .columns
        .iter()
        .map(|c| quoted(c))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {}{cols} FROM {} LIMIT 0",
        if table.has_rowid { "rowid," } else { "" },
        quoted(&table.name)
    );
    connection.with_client(|client| {
        let prepared = client.prepare(&sql)?;
        Ok(prepared
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_().clone()))
            .collect())
    })
}
fn copy_table(
    source: &Sqlite,
    target: &Connection,
    table: &SourceTable,
    deadline: Instant,
) -> Result<()> {
    let target_columns = target_columns(target, table)?;
    let columns = target_columns
        .iter()
        .map(|(n, _)| quoted(n))
        .collect::<Vec<_>>()
        .join(",");
    let types = target_columns
        .iter()
        .map(|(_, t)| t.clone())
        .collect::<Vec<_>>();
    let mut statement = source.prepare(&source_sql(table)).map_err(sqlite_error)?;
    let mut rows = statement.query([]).map_err(sqlite_error)?;
    target.with_client(|client| {
        let sink = client.copy_in(&format!(
            "COPY {} ({columns}) FROM STDIN BINARY",
            quoted(&table.name)
        ))?;
        let mut writer = BinaryCopyInWriter::new(sink, &types);
        let mut copied=0_u64;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            if copied.is_multiple_of(1024) && Instant::now()>=deadline{return Err(Error::Migration("offline migration exceeded its 30 minute operation deadline; target rolled back".into()));}
            let mut values = Vec::with_capacity(target_columns.len());
            for (index, (column, ty)) in target_columns.iter().enumerate() {
                values.push(source_cell(
                    row.get_ref(index).map_err(sqlite_error)?,
                    *ty == Type::JSONB && is_json(&table.name, column),
                )?);
            }
            let refs = values
                .iter()
                .map(|v| v as &(dyn ToSql + Sync))
                .collect::<Vec<_>>();
            writer.write(&refs)?;
            copied+=1;
        }
        writer.finish()?;
        Ok(())
    })
}
fn target_report(target: &Connection, table: &SourceTable) -> Result<TableReport> {
    target_report_version(target, table, false)
}
fn target_report_version(
    target: &Connection,
    table: &SourceTable,
    normalize_version: bool,
) -> Result<TableReport> {
    let cols = table
        .columns
        .iter()
        .map(|c| {
            if normalize_version && table.name == "meta" && c == "value" {
                format!(
                    "CASE WHEN key='schema_version' THEN '{}' ELSE value END AS value",
                    crate::schema::LEGACY_SCHEMA_VERSION
                )
            } else {
                quoted(c)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = if table.has_rowid {
        format!(
            "SELECT rowid,{cols} FROM {} ORDER BY rowid",
            quoted(&table.name)
        )
    } else {
        format!("SELECT {cols} FROM {} ORDER BY {cols}", quoted(&table.name))
    };
    target.with_client(|client| {
        let mut rows = client.query_raw(&sql, std::iter::empty::<&(dyn ToSql + Sync)>())?;
        let mut digest = Sha256::new();
        let mut count = 0;
        let mut blob_bytes = 0;
        // postgres RowIter streams from the server instead of materializing
        // the complete database in process memory.
        use postgres::fallible_iterator::FallibleIterator;
        while let Some(row) = rows.next()? {
            let row = crate::query::Row::from_postgres(row);
            for index in 0..table.columns.len() + usize::from(table.has_rowid) {
                digest_cell(&mut digest, &row.get_value(index)?, &mut blob_bytes)?;
            }
            digest.update([0xff]);
            count += 1;
        }
        Ok(TableReport {
            table: table.name.clone(),
            columns: table.columns.clone(),
            rows: count,
            checksum_sha256: crate::hex(&digest.finalize()),
            blob_bytes,
            has_rowid: table.has_rowid,
        })
    })
}
fn assert_pristine(connection: &Connection) -> Result<Vec<String>> {
    const INTERNAL: &[&str] = &[
        "dc2_schema_migrations",
        "dc2_bootstrap_defaults",
        "dc2_metric_partitions",
        "dc2_delivery_protection_heads",
        "dc2_storage_lease_artifacts",
        "dc2_review_run_refs",
        "dc2_active_review_runs",
        "dc2_test_log_retention_heads",
        "dc2_test_log_retention_runs",
        "dc2_sqlite_import_receipts",
    ];
    let all_tables=connection.prepare("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') AND NOT c.relispartition ORDER BY c.relname")?.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>>>()?;
    for table in &all_tables {
        if table.starts_with("dc2_") && !INTERNAL.contains(&table.as_str()) {
            return Err(Error::Migration(
                "target contains an unreviewed internal table; refusing overwrite".into(),
            ));
        }
        if table == "dc2_sqlite_import_receipts" {
            return Err(Error::Migration(
                "target already has an import receipt; refusing overwrite".into(),
            ));
        }
        if INTERNAL.contains(&table.as_str())
            && !matches!(
                table.as_str(),
                "dc2_schema_migrations" | "dc2_bootstrap_defaults"
            )
            && connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {})", quoted(table)),
                [],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Err(Error::Migration(
                "target has existing derived authority state; refusing overwrite".into(),
            ));
        }
    }
    let tables = all_tables
        .into_iter()
        .filter(|table| !INTERNAL.contains(&table.as_str()))
        .collect::<Vec<_>>();
    let defaults = BTreeMap::from([
        ("meta", 2_i64),
        ("usage_rate_cards", 10),
        ("ticket_settings", 1),
        ("storage_scan_state", 1),
        ("test_log_retention_state", 1),
    ]);
    for table in &tables {
        let expected = defaults.get(table.as_str()).copied().unwrap_or(0);
        let count: i64 = connection.query_row(
            &format!(
                "SELECT count(*) FROM(SELECT 1 FROM {} LIMIT $1) bounded",
                quoted(table)
            ),
            [expected + 1],
            |r| r.get(0),
        )?;
        if count != expected {
            return Err(Error::Migration(format!(
                "target is not a pristine authority schema ({table}); refusing overwrite"
            )));
        }
    }
    for table in defaults.keys() {
        let matches:bool=connection.query_row(&format!("SELECT (SELECT rows_json FROM dc2_bootstrap_defaults WHERE table_name=$1) = (SELECT jsonb_agg(to_jsonb(t) ORDER BY rowid) FROM {} t)",quoted(table)),[*table],|r|r.get(0))?;
        if !matches {
            return Err(Error::Migration(
                "target defaults were modified; refusing overwrite".into(),
            ));
        }
    }
    if connection.query_row(
        "SELECT to_regclass('public.dc2_sqlite_import_receipts') IS NOT NULL",
        [],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(Error::Migration(
            "target already has an import receipt; refusing overwrite".into(),
        ));
    }
    Ok(tables)
}
fn restore_identities(connection: &Connection, source: &SourceReport) -> Result<()> {
    let columns=connection.prepare("SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' AND is_identity='YES' AND table_name IN (SELECT relname FROM pg_class WHERE NOT relispartition AND relnamespace='public'::regnamespace) ORDER BY table_name,column_name")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>>>()?;
    for (table, column) in columns {
        let source_sequence = if column != "rowid" {
            source.sequences.get(&table).copied().unwrap_or(0)
        } else {
            0
        };
        let maximum: Option<i64> = connection.query_row(
            &format!("SELECT max({}) FROM {}", quoted(&column), quoted(&table)),
            [],
            |r| r.get(0),
        )?;
        let value = maximum.unwrap_or(0).max(source_sequence);
        let sequence: String = connection.query_row(
            "SELECT pg_get_serial_sequence($1,$2)",
            params![table, column],
            |r| r.get(0),
        )?;
        connection.query_row(
            "SELECT setval($1::text::regclass,$2,$3::boolean)",
            params![sequence, value.max(1), value > 0],
            |_| Ok(()),
        )?;
    }
    Ok(())
}

/// Import into a provably pristine PostgreSQL schema, in one rollback-safe
/// transaction. Unknown source tables/columns, target writes, type mismatches,
/// source changes, and checksum mismatches all fail closed before publication.
pub fn import_sqlite(database: &Persistence, path: &Path) -> Result<ImportReceipt> {
    let _budget = OfflineBudget::enter();
    let deadline = Instant::now() + OFFLINE_TIMEOUT;
    let source = FrozenSource::open(path)?;
    let source_tables = tables(&source.connection)?;
    let source_report = report(&source, &source_tables)?;
    if source_report.schema_version != Some(crate::schema::LEGACY_SCHEMA_VERSION) {
        return Err(Error::Migration(
            "cutover requires the complete frozen SQLite schema 34".into(),
        ));
    }
    database.migrate()?;
    database.maintenance_transaction(|tx| {
        tx.execute_batch("SET LOCAL statement_timeout='30min'; SET LOCAL idle_in_transaction_session_timeout='30min'")?;
        if Instant::now()>=deadline{return Err(Error::Migration("offline migration deadline reached before copy".into()));}
        tx.query_row("SELECT pg_advisory_xact_lock(29162844963183703)",[],|_|Ok(()))?;
        let target_tables=assert_pristine(tx)?;
        let known=target_tables.iter().map(String::as_str).collect::<BTreeSet<_>>();
        for table in &source_tables{if !known.contains(table.name.as_str()){return Err(Error::Migration(format!("source table {} has no reviewed target; refusing data loss",table.name)));}target_columns(tx,table)?;}
        let source_names=source_tables.iter().map(|t|t.name.as_str()).collect::<BTreeSet<_>>();
        for table in &target_tables{if !matches!(table.as_str(),"coordinator_maintenance"|"test_log_retention_projection"|"generation_source_origins"|"codex_usage_contexts"|"codex_usage_operation_contexts") && !source_names.contains(table.as_str()){return Err(Error::Migration(format!("source is missing required authority table {table}")));}}
        tx.execute_batch(&format!("TRUNCATE {} RESTART IDENTITY",target_tables.iter().map(|t|quoted(t)).chain(["dc2_delivery_protection_heads","dc2_storage_lease_artifacts","dc2_review_run_refs","dc2_active_review_runs","dc2_test_log_retention_heads","dc2_test_log_retention_runs"].iter().map(|t|quoted(t))).collect::<Vec<_>>().join(",")))?;
        tx.execute_batch("SET CONSTRAINTS ALL DEFERRED")?;
        let mut partition_report=crate::maintenance::PartitionReport::default();
        if let Some(table)=source_tables.iter().find(|t|t.name=="metric_minutes") {
            let days=source.connection.prepare("SELECT DISTINCT substr(minute_utc,1,10) FROM metric_minutes ORDER BY 1").map_err(sqlite_error)?.query_map([],|r|r.get::<_,String>(0)).map_err(sqlite_error)?.collect::<std::result::Result<Vec<_>,_>>().map_err(sqlite_error)?;
            let _=table;
            for day in days {if let Ok(date)=time::Date::parse(&day,time::macros::format_description!("[year]-[month]-[day]")){crate::maintenance::ensure_metric_day(tx,date.with_hms(0,0,0).map_err(|e|Error::Migration(e.to_string()))?.assume_utc(),&mut partition_report)?;}}
        }
        for table in &source_tables{copy_table(&source.connection,tx,table,deadline)?;}
        tx.query_row("SELECT dc2_rebuild_review_reminder_projection()",[],|_|Ok(()))?;
        // Rebuild the searchable sketch document from the exact FTS content,
        // which is a derived index rather than an independently owned table.
        if source.connection.query_row("SELECT 1 FROM sqlite_master WHERE name='sketches_fts' AND type='table'",[],|_|Ok(())).is_ok(){
            let mut statement=source.connection.prepare("SELECT sketch_id,searchable FROM sketches_fts ORDER BY rowid").map_err(sqlite_error)?;
            let mut rows=statement.query([]).map_err(sqlite_error)?;
            while let Some(row)=rows.next().map_err(sqlite_error)?{tx.execute("UPDATE sketches SET searchable=$1 WHERE sketch_id=$2",params![row.get::<_,String>(1).map_err(sqlite_error)?,row.get::<_,String>(0).map_err(sqlite_error)?])?;}
        }
        restore_identities(tx,&source_report)?;
        tx.execute_batch("SET CONSTRAINTS ALL IMMEDIATE")?;
        let mut checks=Vec::new();
        for (table,source_check) in source_tables.iter().zip(&source_report.tables){let target=target_report(tx,table)?;let matched=source_check==&target;if !matched{return Err(Error::Migration(format!("typed checksum mismatch in {}; target rolled back",table.name)));}checks.push(TableCheck{source:source_check.clone(),target,matched});}
        if !source.unchanged()?{return Err(Error::Migration("source changed during migration; target rolled back".into()));}
        let mut receipt=ImportReceipt{schema:1,source:source_report.clone(),checks,source_unchanged:true,foreign_keys_valid:true,identities_restored:true,committed:true,source_retained:true,receipt_sha256:String::new()};
        receipt.receipt_sha256=crate::hex(&Sha256::digest(serde_json::to_vec(&receipt).map_err(|e|Error::Migration(e.to_string()))?));
        tx.execute_batch("CREATE TABLE dc2_sqlite_import_receipts(receipt_sha256 TEXT PRIMARY KEY,receipt_json JSONB NOT NULL,imported_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()); CREATE TRIGGER dc2_import_receipt_no_update BEFORE UPDATE OR DELETE ON dc2_sqlite_import_receipts FOR EACH ROW EXECUTE FUNCTION dc2_reject_history_mutation()")?;
        tx.execute("INSERT INTO dc2_sqlite_import_receipts(receipt_sha256,receipt_json) VALUES($1,$2)",params![receipt.receipt_sha256,serde_json::to_value(&receipt).map_err(|e|Error::Migration(e.to_string()))?])?;
        tx.execute("UPDATE meta SET value=$1 WHERE key='schema_version'",[crate::schema::DATABASE_SCHEMA_VERSION.to_string()])?;
        Ok(receipt)
    })
}

/// Recheck the committed import after reconnect or server restart. This is
/// read-only and rejects any missing, changed or extra source-owned row.
pub fn verify_import(database: &Persistence, path: &Path) -> Result<ImportReceipt> {
    let _budget = OfflineBudget::enter();
    let source = FrozenSource::open(path)?;
    let tables = tables(&source.connection)?;
    let observed = report(&source, &tables)?;
    database.maintenance_transaction(|connection| {
        connection
            .execute_batch("SET TRANSACTION READ ONLY; SET LOCAL statement_timeout='30min'; SET LOCAL idle_in_transaction_session_timeout='30min'")?;
        let stored: String = connection.query_row(
            "SELECT receipt_json FROM dc2_sqlite_import_receipts ORDER BY imported_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )?;
        let receipt: ImportReceipt = serde_json::from_str(&stored)
            .map_err(|_| Error::Migration("import receipt is invalid".into()))?;
        if receipt.source != observed {
            return Err(Error::Migration(
                "source no longer matches its committed import receipt".into(),
            ));
        }
        for (table, source_check) in tables.iter().zip(&observed.tables) {
            if target_report_version(connection, table, true)? != *source_check {
                return Err(Error::Migration(format!(
                    "post-restart preservation check failed for {}",
                    table.name
                )));
            }
        }
        if !source.unchanged()? {
            return Err(Error::Migration(
                "source changed during post-restart verification".into(),
            ));
        }
        Ok(receipt)
    })
}
