//! Read-only planning metadata from an explicitly selected activation backup.
//! This never opens live authority, migrates or restores data. The bounded internal
//! reader shares the same before/after identity and hash checks with recovery.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_HEADER_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct InspectRequest {
    pub transaction_dir: PathBuf,
    pub repository_id: String,
    pub task_ids: Vec<String>,
    pub include_identities: bool,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub version: u8,
    pub transaction: String,
    pub snapshot_schema: u8,
    pub snapshot_status: String,
    pub provenance: &'static str,
    pub backup_sha256: String,
    pub database_schema: u32,
    pub repository_id: String,
    pub repository_present: bool,
    pub tasks: i64,
    pub decisions: i64,
    pub releases: i64,
    pub plan_events: i64,
    pub requested_tasks: Vec<TaskPresence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identities: Option<PlanningIdentities>,
    pub recovery_performed: bool,
}

#[derive(Debug, Serialize)]
pub struct PlanningIdentities {
    pub tasks: Vec<RecordIdentity>,
    pub decisions: Vec<RecordIdentity>,
    pub releases: Vec<RecordIdentity>,
}

#[derive(Debug, Serialize)]
pub struct RecordIdentity {
    pub id: String,
    pub sequence: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskPresence {
    pub task_id: String,
    pub repository_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<TaskState>,
}

#[derive(Debug, Serialize)]
pub struct TaskState {
    pub status: devcoordinator2_api::params::TaskStatus,
    pub updated_at: String,
    pub position: i64,
}

// Unknown fields include private installation contents. Do not retain or emit them.
#[derive(Deserialize)]
struct SnapshotHeader {
    schema: u8,
    status: String,
    transaction_dir: PathBuf,
    backup_sha256: Option<String>,
}

pub fn inspect(request: &InspectRequest) -> Result<Inspection, String> {
    inspect_inner(request, || {})
}

pub fn inspect_with<T>(
    request: &InspectRequest,
    read: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<(Inspection, T), String> {
    inspect_with_inner(request, || {}, read)
}

fn inspect_inner(
    request: &InspectRequest,
    after_query: impl FnOnce(),
) -> Result<Inspection, String> {
    inspect_with_inner(request, after_query, |_| Ok(())).map(|(inspection, ())| inspection)
}

fn inspect_with_inner<T>(
    request: &InspectRequest,
    after_query: impl FnOnce(),
    read: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<(Inspection, T), String> {
    if !valid_id(&request.repository_id, b'r')
        || request.task_ids.len() > 64
        || request.task_ids.iter().any(|id| !valid_id(id, b'p'))
    {
        return Err("use one repository ID and at most 64 valid task IDs".into());
    }
    let root = &request.transaction_dir;
    if !root.is_absolute()
        || fs::canonicalize(root).map_err(|_| "activation directory is unavailable")? != *root
    {
        return Err("use the physical absolute activation directory, without aliases".into());
    }
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| "activation directory is unavailable")?;
    if !root_metadata.is_dir() {
        return Err("activation path must be a directory".into());
    }
    let header_path = root.join("installation-snapshot.json");
    let backup_path = root.join("authority-before.sqlite3");
    let mut header_file = open_private(&header_path)?;
    let mut header_bytes = Vec::new();
    (&mut header_file)
        .take(MAX_HEADER_BYTES + 1)
        .read_to_end(&mut header_bytes)
        .map_err(|_| "cannot read activation snapshot")?;
    if header_bytes.len() as u64 > MAX_HEADER_BYTES {
        return Err("activation snapshot exceeds the inspection limit".into());
    }
    let header: SnapshotHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| "invalid activation snapshot header")?;
    if !matches!(header.schema, 1 | 2)
        || header.transaction_dir != *root
        || !matches!(
            header.status.as_str(),
            "prepared" | "committed" | "rolling_back" | "rolled_back" | "recovered"
        )
    {
        return Err("unsupported activation snapshot or mismatched transaction identity".into());
    }
    let mut backup_file = open_private(&backup_path)?;
    let backup_sha256 = hash(&mut backup_file)?;
    let provenance = match header.backup_sha256.as_deref() {
        Some(expected) if valid_hash(expected) && expected == backup_sha256 => {
            "recorded_hash_matched"
        }
        Some(_) => return Err("backup differs from its recorded activation hash".into()),
        None if header.schema == 1 => "legacy_without_recorded_hash",
        None => return Err("activation snapshot has no required backup hash".into()),
    };

    // Immutable is appropriate only for this saved snapshot. It avoids WAL/SHM
    // creation and never performs recovery or schema migration on the input.
    let uri = format!("file:{}?mode=ro&immutable=1", uri_path(&backup_path)?);
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .map_err(|_| "cannot open the saved backup read-only")?;
    connection
        .execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(|_| "cannot configure read-only inspection")?;
    let started = Instant::now();
    connection
        .progress_handler(
            10_000,
            Some(move || started.elapsed() > Duration::from_secs(30)),
        )
        .map_err(|_| "cannot set the bounded inspection callback")?;
    let integrity: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "saved backup integrity could not be checked")?;
    if integrity != "ok" {
        return Err("saved backup failed SQLite integrity checking".into());
    }
    for name in [
        "meta",
        "repositories",
        "tasks",
        "decisions",
        "releases",
        "plan_events",
    ] {
        let kind: Option<String> = connection
            .query_row(
                "SELECT type FROM sqlite_schema WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "cannot inspect the backup table contract")?;
        if kind.as_deref() != Some("table") {
            return Err(format!("backup has no ordinary {name} table"));
        }
    }
    let schema: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "backup schema metadata is unavailable")?;
    let database_schema: u32 = schema
        .parse()
        .map_err(|_| "invalid backup schema version")?;
    if database_schema == 0 || database_schema > devcoordinator2_api::DATABASE_SCHEMA_VERSION {
        return Err("backup database schema is unsupported; no migration was attempted".into());
    }
    let count = |sql: &str| -> Result<i64, String> {
        connection
            .query_row(sql, [&request.repository_id], |row| row.get(0))
            .map_err(|_| "cannot read repository-scoped planning counts")
            .map_err(str::to_owned)
    };
    let mut requested_tasks = Vec::new();
    for id in &request.task_ids {
        let repository_id: Option<String> = connection
            .query_row(
                "SELECT repository_id FROM tasks WHERE task_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "cannot inspect a requested task ID")?;
        requested_tasks.push(TaskPresence {
            task_id: id.clone(),
            state: if repository_id.as_deref() == Some(request.repository_id.as_str()) {
                read_task_state(&connection, id)?
            } else {
                None
            },
            repository_id,
        });
    }
    let result = Inspection {
        version: 1,
        transaction: root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid activation directory name")?
            .to_owned(),
        snapshot_schema: header.schema,
        snapshot_status: header.status,
        provenance,
        backup_sha256: backup_sha256.clone(),
        database_schema,
        repository_id: request.repository_id.clone(),
        repository_present: count("SELECT COUNT(*) FROM repositories WHERE repository_id=?1")? != 0,
        tasks: count("SELECT COUNT(*) FROM tasks WHERE repository_id=?1")?,
        decisions: count("SELECT COUNT(*) FROM decisions WHERE repository_id=?1")?,
        releases: count("SELECT COUNT(*) FROM releases WHERE repository_id=?1")?,
        plan_events: count("SELECT COUNT(*) FROM plan_events WHERE repository_id=?1")?,
        requested_tasks,
        identities: if request.include_identities {
            Some(PlanningIdentities {
                tasks: read_identities(
                    &connection,
                    &request.repository_id,
                    "SELECT task_id,seq,NULL FROM tasks WHERE repository_id=?1 ORDER BY seq LIMIT 4097",
                    b'p',
                )?,
                decisions: read_identities(
                    &connection,
                    &request.repository_id,
                    "SELECT decision_id,seq,ref FROM decisions WHERE repository_id=?1 ORDER BY seq LIMIT 4097",
                    b'n',
                )?,
                releases: read_identities(
                    &connection,
                    &request.repository_id,
                    "SELECT release_id,seq,NULL FROM releases WHERE repository_id=?1 ORDER BY seq LIMIT 4097",
                    b'v',
                )?,
            })
        } else {
            None
        },
        recovery_performed: false,
    };
    let payload = read(&connection)?;
    drop(connection);
    after_query();
    if hash(&mut backup_file)? != backup_sha256
        || hash(&mut header_file)? != hex(&Sha256::digest(&header_bytes))
        || !same_file(&backup_file, &backup_path)?
        || !same_file(&header_file, &header_path)?
    {
        return Err("activation backup changed during inspection; discard the observation".into());
    }
    Ok((result, payload))
}

fn read_task_state(connection: &Connection, id: &str) -> Result<Option<TaskState>, String> {
    let mut statement = connection
        .prepare("PRAGMA table_info(tasks)")
        .map_err(|_| "cannot inspect task metadata columns")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| "cannot inspect task metadata columns")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "cannot inspect task metadata columns")?;
    if ["status", "updated_at", "position"]
        .iter()
        .any(|column| !columns.iter().any(|value| value == column))
    {
        return Ok(None);
    }
    let (status, updated_at, position): (String, String, i64) = connection
        .query_row(
            "SELECT status,updated_at,position FROM tasks WHERE task_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| "cannot inspect requested task state")?;
    let status = serde_json::from_value(serde_json::Value::String(status))
        .map_err(|_| "requested task has an invalid status")?;
    if updated_at.len() > 64
        || time::OffsetDateTime::parse(&updated_at, &time::format_description::well_known::Rfc3339)
            .is_err()
    {
        return Err("requested task has an invalid update timestamp".into());
    }
    Ok(Some(TaskState {
        status,
        updated_at,
        position,
    }))
}

fn valid_id(value: &str, prefix: u8) -> bool {
    value.len() == 17
        && value.as_bytes()[0] == prefix
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

fn read_identities(
    connection: &Connection,
    repository_id: &str,
    sql: &str,
    prefix: u8,
) -> Result<Vec<RecordIdentity>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| "backup identity columns are unavailable")?;
    let rows = statement
        .query_map([repository_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| "cannot query scoped planning identities")?;
    let mut identities = Vec::new();
    for row in rows {
        let (id, sequence, reference) = row.map_err(|_| "invalid saved planning identity")?;
        if !valid_id(&id, prefix) || sequence < 1 {
            return Err("invalid saved planning identity".into());
        }
        identities.push(RecordIdentity {
            id,
            sequence,
            reference_sha256: reference.map(|value| hex(&Sha256::digest(value.as_bytes()))),
        });
    }
    if identities.len() > 4096 {
        return Err("planning identity catalogue exceeds the bounded inspection limit".into());
    }
    Ok(identities)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn open_private(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "saved snapshot file is unavailable or is an alias")?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect saved snapshot identity")?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err("saved snapshot must be a private owned regular file".into());
    }
    Ok(file)
}

fn same_file(file: &File, path: &Path) -> Result<bool, String> {
    let opened = file
        .metadata()
        .map_err(|_| "cannot recheck open snapshot identity")?;
    let current = fs::symlink_metadata(path).map_err(|_| "saved snapshot disappeared")?;
    Ok(current.is_file() && opened.dev() == current.dev() && opened.ino() == current.ino())
}

fn hash(file: &mut File) -> Result<String, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "cannot seek saved snapshot")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = file
            .read(&mut buffer)
            .map_err(|_| "cannot hash saved snapshot")?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
    }
    Ok(hex(&digest.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn uri_path(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("saved snapshot path is not UTF-8")?;
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const REPO: &str = "r946ed77e45b31d74";
    const TASK: &str = "p9966151ec04cd9cf";
    const OTHER: &str = "p463bdb46af5571c0";

    fn fixture(schema: u8) -> (tempfile::TempDir, InspectRequest) {
        let directory = tempfile::Builder::new()
            .prefix("planning backup % ")
            .tempdir()
            .unwrap();
        let backup = directory.path().join("authority-before.sqlite3");
        let db = Connection::open(&backup).unwrap();
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO meta VALUES('schema_version','20');
            CREATE TABLE repositories(repository_id TEXT PRIMARY KEY); INSERT INTO repositories VALUES('r946ed77e45b31d74');
            CREATE TABLE tasks(task_id TEXT PRIMARY KEY,repository_id TEXT,title TEXT,seq INTEGER);
            INSERT INTO tasks VALUES('p9966151ec04cd9cf','r946ed77e45b31d74','PRIVATE_TASK_SENTINEL',1);
            INSERT INTO tasks VALUES('p463bdb46af5571c0','r0000000000000001','UNRELATED_SENTINEL',1);
            CREATE TABLE decisions(repository_id TEXT,decision_id TEXT,seq INTEGER,ref TEXT);
            INSERT INTO decisions VALUES('r946ed77e45b31d74','n0000000000000001',1,'PRIVATE_REF_SENTINEL');
            CREATE TABLE releases(repository_id TEXT,release_id TEXT,seq INTEGER);
            INSERT INTO releases VALUES('r946ed77e45b31d74','v0000000000000001',1);
            CREATE TABLE plan_events(repository_id TEXT); INSERT INTO plan_events VALUES('r946ed77e45b31d74');
            CREATE TABLE private_authority(value TEXT); INSERT INTO private_authority VALUES('CREDENTIAL_SENTINEL');").unwrap();
        drop(db);
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
        let mut header = json!({"schema":schema,"status":"committed","transaction_dir":directory.path(),
            "entries":[{"content_base64":"PRIVATE_INSTALLATION_SENTINEL"}]});
        if schema == 2 {
            header["backup_sha256"] = json!(hash(&mut File::open(&backup).unwrap()).unwrap());
        }
        write_header(directory.path(), header);
        let request = InspectRequest {
            transaction_dir: directory.path().to_owned(),
            repository_id: REPO.into(),
            task_ids: vec![TASK.into(), OTHER.into()],
            include_identities: false,
        };
        (directory, request)
    }

    fn write_header(root: &Path, header: serde_json::Value) {
        let path = root.join("installation-snapshot.json");
        fs::write(&path, serde_json::to_vec(&header).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn verified_inspection_is_scoped_metadata_and_changes_no_files() {
        let (root, request) = fixture(2);
        let backup = root.path().join("authority-before.sqlite3");
        let before = fs::read(&backup).unwrap();
        let result = inspect(&request).unwrap();
        assert_eq!(result.provenance, "recorded_hash_matched");
        assert_eq!(
            (
                result.tasks,
                result.decisions,
                result.releases,
                result.plan_events
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(
            result.requested_tasks[0].repository_id.as_deref(),
            Some(REPO)
        );
        assert_eq!(
            result.requested_tasks[1].repository_id.as_deref(),
            Some("r0000000000000001")
        );
        assert!(!result.recovery_performed);
        assert_eq!(before, fs::read(backup).unwrap());
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            2,
            "inspection creates no WAL, SHM or export files"
        );
        let output = serde_json::to_string(&result).unwrap();
        for secret in [
            "PRIVATE_TASK_SENTINEL",
            "UNRELATED_SENTINEL",
            "CREDENTIAL_SENTINEL",
            "PRIVATE_INSTALLATION_SENTINEL",
        ] {
            assert!(
                !output.contains(secret),
                "private record text must not escape"
            );
        }
    }

    #[test]
    fn requested_task_states_are_bounded_and_repository_scoped() {
        let (directory, request) = fixture(1);
        let db = Connection::open(directory.path().join("authority-before.sqlite3")).unwrap();
        db.execute_batch("ALTER TABLE tasks ADD COLUMN status TEXT; ALTER TABLE tasks ADD COLUMN updated_at TEXT; ALTER TABLE tasks ADD COLUMN position INTEGER;
            UPDATE tasks SET status='dropped',updated_at='2026-09-06T12:30:00Z',position=7 WHERE repository_id='r946ed77e45b31d74';
            UPDATE tasks SET status='PRIVATE_STATUS',updated_at='PRIVATE_TIMESTAMP',position=8 WHERE repository_id='r0000000000000001';").unwrap();
        drop(db);
        let result = serde_json::to_value(inspect(&request).unwrap()).unwrap();
        assert_eq!(result["requested_tasks"][0]["state"]["status"], "dropped");
        assert_eq!(
            result["requested_tasks"][0]["state"]["updated_at"],
            "2026-09-06T12:30:00Z"
        );
        assert_eq!(result["requested_tasks"][0]["state"]["position"], 7);
        assert!(result["requested_tasks"][1].get("state").is_none());
        assert!(!result.to_string().contains("PRIVATE_"));
    }

    #[test]
    fn requested_task_state_rejects_private_values_in_metadata_fields() {
        for (status, timestamp) in [
            ("PRIVATE_STATUS", "2026-09-06T12:30:00Z"),
            ("dropped", "PRIVATE_TIMESTAMP"),
        ] {
            let (directory, request) = fixture(1);
            let db = Connection::open(directory.path().join("authority-before.sqlite3")).unwrap();
            db.execute_batch("ALTER TABLE tasks ADD COLUMN status TEXT; ALTER TABLE tasks ADD COLUMN updated_at TEXT; ALTER TABLE tasks ADD COLUMN position INTEGER;").unwrap();
            db.execute(
                "UPDATE tasks SET status=?1,updated_at=?2,position=7 WHERE repository_id=?3",
                rusqlite::params![status, timestamp, REPO],
            )
            .unwrap();
            drop(db);
            let error = inspect(&request).unwrap_err();
            assert!(error.contains("invalid"));
            assert!(!error.contains("PRIVATE_"));
        }
    }

    #[test]
    fn legacy_unhashed_snapshot_is_not_claimed_as_historically_verified() {
        let (_root, request) = fixture(1);
        let result = inspect(&request).unwrap();
        assert_eq!(result.provenance, "legacy_without_recorded_hash");
        assert!(valid_hash(&result.backup_sha256));
    }

    #[test]
    fn opt_in_identities_expose_ids_and_reference_hashes_not_record_text() {
        let (_root, mut request) = fixture(2);
        assert!(inspect(&request).unwrap().identities.is_none());
        request.include_identities = true;
        let result = inspect(&request).unwrap();
        let ids = result.identities.as_ref().unwrap();
        assert_eq!(ids.tasks.len(), 1);
        assert_eq!(ids.tasks[0].id, TASK);
        assert_eq!(
            ids.decisions[0].reference_sha256.as_deref(),
            Some(hex(&Sha256::digest(b"PRIVATE_REF_SENTINEL")).as_str())
        );
        let output = serde_json::to_string(&result).unwrap();
        assert!(!output.contains("PRIVATE_REF_SENTINEL"));
        assert!(!output.contains("PRIVATE_TASK_SENTINEL"));
        assert!(!output.contains("UNRELATED_SENTINEL"));
    }

    #[test]
    fn malformed_saved_ids_are_rejected_before_they_can_expose_private_text() {
        let (root, mut request) = fixture(1);
        let db = Connection::open(root.path().join("authority-before.sqlite3")).unwrap();
        db.execute(
            "UPDATE decisions SET decision_id='PRIVATE_BAD_ID_SENTINEL'",
            [],
        )
        .unwrap();
        drop(db);
        request.include_identities = true;
        let error = inspect(&request).unwrap_err();
        assert!(error.contains("invalid saved planning identity"));
        assert!(!error.contains("PRIVATE_BAD_ID_SENTINEL"));
    }

    #[test]
    fn missing_repository_and_ids_are_explicit_not_other_project_counts() {
        let (_root, mut request) = fixture(2);
        request.repository_id = "r1111111111111111".into();
        request.task_ids = vec!["p1111111111111111".into()];
        let result = inspect(&request).unwrap();
        assert!(!result.repository_present);
        assert_eq!(
            (
                result.tasks,
                result.decisions,
                result.releases,
                result.plan_events
            ),
            (0, 0, 0, 0)
        );
        assert!(result.requested_tasks[0].repository_id.is_none());
    }

    #[test]
    fn recorded_hash_mismatch_and_missing_v2_hash_refuse_before_querying() {
        let (root, request) = fixture(2);
        let path = root.path().join("installation-snapshot.json");
        let mut header: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        header["backup_sha256"] = json!("0".repeat(64));
        write_header(root.path(), header.clone());
        assert!(
            inspect(&request)
                .unwrap_err()
                .contains("recorded activation hash")
        );
        header.as_object_mut().unwrap().remove("backup_sha256");
        write_header(root.path(), header);
        assert!(
            inspect(&request)
                .unwrap_err()
                .contains("required backup hash")
        );
    }

    #[test]
    fn future_database_schema_is_not_migrated_or_partially_reported() {
        let (root, request) = fixture(1);
        let path = root.path().join("authority-before.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE meta SET value='999'", []).unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        assert!(inspect(&request).unwrap_err().contains("no migration"));
        assert_eq!(before, fs::read(path).unwrap());
    }

    #[test]
    fn aliases_and_nonprivate_input_are_rejected() {
        let (root, request) = fixture(1);
        let backup = root.path().join("authority-before.sqlite3");
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(inspect(&request).unwrap_err().contains("private owned"));
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&backup, root.path().join("retained.sqlite3")).unwrap();
        symlink("retained.sqlite3", &backup).unwrap();
        assert!(inspect(&request).is_err());
        let alias_root = tempfile::tempdir().unwrap();
        let alias = alias_root.path().join("alias");
        symlink(root.path(), &alias).unwrap();
        assert!(
            inspect(&InspectRequest {
                transaction_dir: alias,
                ..request
            })
            .unwrap_err()
            .contains("without aliases")
        );
    }

    #[test]
    fn views_and_missing_tables_are_not_accepted_as_the_backup_contract() {
        let (root, request) = fixture(1);
        let db = Connection::open(root.path().join("authority-before.sqlite3")).unwrap();
        db.execute_batch("DROP TABLE tasks; CREATE VIEW tasks AS SELECT 'p9966151ec04cd9cf' AS task_id, 'r946ed77e45b31d74' AS repository_id;").unwrap();
        drop(db);
        assert!(
            inspect(&request)
                .unwrap_err()
                .contains("ordinary tasks table")
        );
    }

    #[test]
    fn changed_backup_during_inspection_discards_the_observation() {
        let (root, request) = fixture(1);
        let result = inspect_inner(&request, || {
            fs::write(root.path().join("authority-before.sqlite3"), b"changed").unwrap();
        });
        assert!(result.unwrap_err().contains("changed during inspection"));
    }

    #[test]
    fn malformed_database_is_not_repaired_or_reported_as_empty() {
        let (root, request) = fixture(1);
        let path = root.path().join("authority-before.sqlite3");
        fs::write(&path, b"not a SQLite database").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(inspect(&request).is_err());
        assert_eq!(before, fs::read(path).unwrap());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn changed_snapshot_header_discards_the_observation() {
        let (root, request) = fixture(1);
        let result = inspect_inner(&request, || {
            write_header(
                root.path(),
                json!({"schema":1,"status":"rolled_back","transaction_dir":root.path()}),
            );
        });
        assert!(result.unwrap_err().contains("changed during inspection"));
    }

    #[test]
    fn replacing_the_backup_with_identical_bytes_still_changes_its_identity() {
        let (root, request) = fixture(1);
        let backup = root.path().join("authority-before.sqlite3");
        let replacement = root.path().join("replacement.sqlite3");
        fs::copy(&backup, &replacement).unwrap();
        let result = inspect_inner(&request, || {
            fs::rename(replacement, backup).unwrap();
        });
        assert!(result.unwrap_err().contains("changed during inspection"));
    }

    #[test]
    fn invalid_ids_and_mismatched_snapshot_identity_are_rejected() {
        let (root, mut request) = fixture(1);
        request.repository_id = "not-an-id".into();
        assert!(inspect(&request).unwrap_err().contains("repository ID"));
        request.repository_id = REPO.into();
        write_header(
            root.path(),
            json!({"schema":1,"status":"committed","transaction_dir":"/another-transaction"}),
        );
        assert!(
            inspect(&request)
                .unwrap_err()
                .contains("transaction identity")
        );
    }
}
