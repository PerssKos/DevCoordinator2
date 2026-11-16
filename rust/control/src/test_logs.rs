//! Typed in-process access to the Rust governed-log store.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod retention;

use crate::database::OptionalExtension;
use devcoordinator2_api::results::{Retention, RetentionDefaults};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use devcoordinator2_executor_core::log_query::{
    LogQueryError, LogQueryOperation, LogQueryOptions, LogQueryRequest, LogQueryResult,
    LogQuerySelector, execute_log_query,
};
use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
use serde_json::{Map, Value};
use time::{format_description::FormatItem, macros::format_description};
use tokio::sync::Notify;

use crate::access::Caller;
use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;

pub const DEFAULT_MAX_AGE_SECONDS: u32 = 86_400;
pub const DEFAULT_CASE_DEPTH: u16 = 3;
const SUMMARY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MAINTENANCE_ERRORS: usize = 64;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceError {
    pub repository_id: String,
    pub code: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceReport {
    pub removed_leaf_folders: u64,
    pub retained_active: u64,
    pub next_expiry_at: Option<String>,
    pub skipped_missing_worktrees: u64,
    pub errors: Vec<MaintenanceError>,
    pub errors_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceStep {
    pub report: MaintenanceReport,
    pub next_worktree_id: Option<String>,
    pub complete: bool,
    pub visited_entries: u64,
    pub processed_bytes: u64,
}

#[derive(Clone)]
pub struct TestLogService {
    database: Database,
    registry: Registry,
    clock: Arc<dyn Clock>,
    maintenance_wake: Arc<Notify>,
}

impl TestLogService {
    pub fn new(database: Database, registry: Registry) -> Self {
        Self::with_clock(database, registry, Arc::new(HostClock))
    }

    pub fn with_clock(database: Database, registry: Registry, clock: Arc<dyn Clock>) -> Self {
        Self {
            database,
            registry,
            clock,
            maintenance_wake: Arc::new(Notify::new()),
        }
    }

    pub fn retention(&self) -> Result<Retention, ProtocolError> {
        self.database
            .read(|connection| {
                connection
                    .query_row(
                        "SELECT max_age_seconds,case_depth,updated_at,updated_by,last_cleanup_at,last_cleanup_error_code FROM test_log_retention_state WHERE singleton=1",
                        [],
                        |row| {
                            Ok(Retention {
                                max_age_seconds: row.get(0)?,
                                case_depth: row.get(1)?,
                                defaults: RetentionDefaults {
                                    max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
                                    case_depth: DEFAULT_CASE_DEPTH,
                                },
                                updated_at: row.get(2)?,
                                updated_by: row.get(3)?,
                                last_cleanup_at: row.get(4)?,
                                last_cleanup_error_code: row.get(5)?,
                                cleanup_requested: None,
                            })
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "log retention state is unavailable",
                )
            })
    }

    pub fn set_retention(
        &self,
        max_age_seconds: u32,
        case_depth: u16,
        actor: &str,
        now: &str,
    ) -> Result<Retention, ProtocolError> {
        let before = self.retention()?;
        if before.max_age_seconds != max_age_seconds || before.case_depth != case_depth {
            let actor = actor.to_owned();
            let now = now.to_owned();
            self.database
                .transaction(move |transaction| {
                    transaction.execute(
                        "UPDATE test_log_retention_state SET max_age_seconds=$1,case_depth=$2,updated_at=$3,updated_by=$4 WHERE singleton=1",
                        crate::database::params![max_age_seconds, case_depth, now, actor],
                    )?;
                    transaction.execute(
                        "INSERT INTO test_log_retention_events(at,actor,previous_max_age_seconds,max_age_seconds,previous_case_depth,case_depth) VALUES($1,$2,$3,$4,$5,$6)",
                        crate::database::params![
                            now,
                            actor,
                            before.max_age_seconds,
                            max_age_seconds,
                            before.case_depth,
                            case_depth,
                        ],
                    )?;
                    Ok(())
                })
                .map_err(database_error)?;
        }
        let mut result = self.retention()?;
        result.cleanup_requested = Some(true);
        self.request_cleanup();
        Ok(result)
    }

    pub fn request_cleanup(&self) {
        self.maintenance_wake.notify_one();
    }
    pub fn maintenance_signal(&self) -> Arc<Notify> {
        self.maintenance_wake.clone()
    }

    pub fn notify_run_finished(&self, _worktree: &Path, _run_id: &str) {
        self.request_cleanup();
    }

    #[cfg(test)]
    pub(crate) fn drain_maintenance_for_test(&self) -> Result<MaintenanceReport, ProtocolError> {
        let mut cursor = None;
        let mut report = MaintenanceReport::default();
        loop {
            let step = self.run_maintenance_step(cursor, 1, Duration::from_millis(100))?;
            report.removed_leaf_folders = report
                .removed_leaf_folders
                .saturating_add(step.report.removed_leaf_folders);
            report.retained_active = report
                .retained_active
                .saturating_add(step.report.retained_active);
            report.skipped_missing_worktrees = report
                .skipped_missing_worktrees
                .saturating_add(step.report.skipped_missing_worktrees);
            report.next_expiry_at = report
                .next_expiry_at
                .into_iter()
                .chain(step.report.next_expiry_at)
                .min();
            report.errors_truncated |= step.report.errors_truncated
                || report.errors.len().saturating_add(step.report.errors.len())
                    > MAX_MAINTENANCE_ERRORS;
            report.errors.extend(step.report.errors);
            report.errors.truncate(MAX_MAINTENANCE_ERRORS);
            if step.complete {
                return Ok(report);
            }
            cursor = step.next_worktree_id;
        }
    }

    /// One bounded authority page. The caller schedules the returned cursor.
    pub fn run_maintenance_step(
        &self,
        after_worktree_id: Option<String>,
        max_worktrees: u16,
        budget: Duration,
    ) -> Result<MaintenanceStep, ProtocolError> {
        retention::step(self, after_worktree_id, max_worktrees, budget)
    }

    pub fn query(
        &self,
        operation: LogQueryOperation,
        path: &Path,
        public_params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let (worktree, repository_id) = self.resolve(path, caller)?;
        let mut object = public_params.as_object().cloned().ok_or_else(|| {
            ProtocolError::new(ErrorCode::ParamsInvalid, "log parameters must be an object")
        })?;
        object.remove("path");
        let mut selector = Map::new();
        let mut options = Map::new();
        for key in ["run_id", "check", "phase", "case", "stream"] {
            if let Some(value) = object.remove(key)
                && !value.is_null()
            {
                selector.insert(key.to_owned(), value);
            }
        }
        if let Some(value) = object.remove("cursor")
            && !value.is_null()
        {
            options.insert("cursor".to_owned(), value);
        }
        for (key, value) in object {
            if !value.is_null() {
                options.insert(key, value);
            }
        }
        if operation == LogQueryOperation::Catalog {
            let retention = self.retention()?;
            options.insert(
                "max_age_seconds".to_owned(),
                Value::from(retention.max_age_seconds),
            );
            options.insert("case_depth".to_owned(), Value::from(retention.case_depth));
        }
        let selector: LogQuerySelector = serde_json::from_value(Value::Object(selector))
            .map_err(|error| invalid_query(error.to_string()))?;
        let options: LogQueryOptions = serde_json::from_value(Value::Object(options))
            .map_err(|error| invalid_query(error.to_string()))?;
        let result = execute_log_query(
            &worktree,
            LogQueryRequest {
                schema: 2,
                operation,
                repository_id,
                selector,
                options,
            },
        )
        .map_err(log_error)?;
        encode_typed_result(operation, result)
    }

    fn resolve(&self, path: &Path, caller: &Caller) -> Result<(PathBuf, String), ProtocolError> {
        if !path.is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be absolute",
            ));
        }
        if caller.is_console() {
            let path = path.to_string_lossy().to_string();
            return self
                .database
                .read(move |connection| {
                    connection
                        .query_row(
                            "SELECT worktree_path,repository_id FROM worktrees WHERE worktree_path=$1",
                            [&path],
                            |row| Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?)),
                        )
                        .optional()
                        .map_err(DatabaseError::from)
                })
                .map_err(database_error)?
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryNotFound,
                        "no registered worktree matches this request",
                    )
                });
        }
        let status = self
            .registry
            .repository_status(path, Some((caller.uid, caller.gid)))?;
        let worktree = status
            .worktrees
            .iter()
            .find(|worktree| worktree.worktree_id == status.worktree_id)
            .map(|worktree| PathBuf::from(&worktree.worktree_path))
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    "the resolved worktree is not registered",
                )
            })?;
        Ok((worktree, status.repository_id))
    }
}

fn push_maintenance_error(
    errors: &mut Vec<MaintenanceError>,
    total: &mut usize,
    repository_id: String,
) {
    *total = total.saturating_add(1);
    if errors.len() < MAX_MAINTENANCE_ERRORS {
        errors.push(MaintenanceError {
            repository_id,
            code: ErrorCode::TestLogUnavailable.to_string(),
        });
    }
}

pub(crate) fn active_run_id(worktree: &Path) -> Result<Option<String>, LogQueryError> {
    let worktree = open(
        worktree,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| LogQueryError::Unavailable)?;
    let Some(devcoordinator) = optional_directory(&worktree, ".devcoordinator")? else {
        return Ok(None);
    };
    let Some(test) = optional_directory(&devcoordinator, "test")? else {
        return Ok(None);
    };
    let Some(current) = optional_directory(&test, "current")? else {
        return Ok(None);
    };
    let summary = match openat(
        &current,
        "summary.json",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => File::from(descriptor),
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(_) => return Err(LogQueryError::Unavailable),
    };
    let metadata = fstat(&summary).map_err(|_| LogQueryError::Unavailable)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_size < 0
        || metadata.st_size as u64 > SUMMARY_BYTES
    {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut bytes = Vec::with_capacity(metadata.st_size as usize);
    summary
        .take(SUMMARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LogQueryError::Unavailable)?;
    if bytes.len() as u64 > SUMMARY_BYTES {
        return Err(LogQueryError::StoreMalformed);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| LogQueryError::StoreMalformed)?;
    if value.get("status").and_then(Value::as_str) != Some("running") {
        return Ok(None);
    }
    let run_id = value
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|run_id| valid_run_id(run_id))
        .ok_or(LogQueryError::StoreMalformed)?;
    Ok(Some(run_id.to_owned()))
}

fn optional_directory(parent: &File, name: &str) -> Result<Option<File>, LogQueryError> {
    match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(_) => Err(LogQueryError::Unavailable),
    }
}

fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && run_id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && b"._-".contains(&byte))
        })
}

fn timestamp(clock: &dyn Clock) -> Result<String, ProtocolError> {
    clock.now_utc().format(TIMESTAMP_FORMAT).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot format log timestamp")
            .with_detail(error.to_string())
    })
}

fn encode_typed_result(
    operation: LogQueryOperation,
    result: LogQueryResult,
) -> Result<Value, ProtocolError> {
    let value = serde_json::to_value(result).map_err(|error| {
        ProtocolError::new(ErrorCode::TestLogUnavailable, "cannot encode log result")
            .with_detail(error.to_string())
    })?;
    match operation {
        LogQueryOperation::Catalog => round_trip::<devcoordinator2_api::results::LogCatalog>(value),
        LogQueryOperation::Tail | LogQueryOperation::Range => {
            round_trip::<devcoordinator2_api::results::LogContent>(value)
        }
        LogQueryOperation::Search => round_trip::<devcoordinator2_api::results::LogSearch>(value),
        LogQueryOperation::FailureContext => {
            round_trip::<devcoordinator2_api::results::LogFailureContext>(value)
        }
    }
}

fn round_trip<T>(value: Value) -> Result<Value, ProtocolError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let typed: T = serde_json::from_value(value).map_err(|error| {
        ProtocolError::new(
            ErrorCode::TestLogUnavailable,
            "the governed log result did not match its public contract",
        )
        .with_detail(error.to_string())
    })?;
    serde_json::to_value(typed).map_err(|error| {
        ProtocolError::new(ErrorCode::TestLogUnavailable, "cannot encode log result")
            .with_detail(error.to_string())
    })
}

fn invalid_query(detail: String) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, "The log request is invalid.").with_detail(detail)
}

fn log_error(error: LogQueryError) -> ProtocolError {
    let (code, message) = match error {
        LogQueryError::ArgsInvalid => (ErrorCode::ParamsInvalid, "The log request is invalid."),
        LogQueryError::LogNotFound => (
            ErrorCode::LogNotFound,
            "The selected governed test log does not exist.",
        ),
        LogQueryError::LogExpired => (
            ErrorCode::LogExpired,
            "The selected governed test log has expired.",
        ),
        LogQueryError::CursorStale => (
            ErrorCode::CursorStale,
            "The log cursor no longer identifies this stream snapshot.",
        ),
        LogQueryError::StoreMalformed
        | LogQueryError::Unavailable
        | LogQueryError::MaintenanceBusy => (
            ErrorCode::TestLogUnavailable,
            "Governed test logs are unavailable.",
        ),
    };
    ProtocolError::new(code, message)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "log database operation failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_executor_core::{
        LeafLogMetadata, LeafSelector, RunLogLease, RunLogMetadata,
    };
    use devcoordinator2_executor_protocol::{DiagnosticExit, LeafStatus, LogStream, RunStatus};
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;
    use time::macros::datetime;

    #[test]
    fn retention_changes_are_persistent_and_append_only() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let service = TestLogService::new(database.clone(), Registry::new(database.clone()));
        let initial = service.retention().expect("retention");
        assert_eq!(initial.max_age_seconds, DEFAULT_MAX_AGE_SECONDS);
        let changed = service
            .set_retention(1234, 7, "uid:1000", "2026-09-03T12:00:00Z")
            .expect("change");
        assert_eq!((changed.max_age_seconds, changed.case_depth), (1234, 7));
        assert_eq!(changed.cleanup_requested, Some(true));
        let events = database
            .read(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM test_log_retention_events",
                    [],
                    |row| row.get::<_, u32>(0),
                )?)
            })
            .expect("events");
        assert_eq!(events, 1);
    }

    #[test]
    fn core_errors_map_to_stable_public_codes() {
        assert_eq!(
            log_error(LogQueryError::CursorStale).code,
            ErrorCode::CursorStale
        );
        assert_eq!(
            log_error(LogQueryError::StoreMalformed).code,
            ErrorCode::TestLogUnavailable
        );
    }

    #[test]
    fn active_summary_is_nofollow_and_only_running_state_is_protected() {
        let temporary = tempdir().expect("tempdir");
        let current = temporary.path().join(".devcoordinator/test/current");
        std::fs::create_dir_all(&current).expect("current");
        let summary = current.join("summary.json");
        std::fs::write(
            &summary,
            r#"{"status":"running","run_id":"t20260903T120000Z-abcd"}"#,
        )
        .expect("summary");
        assert_eq!(
            active_run_id(temporary.path()).expect("active"),
            Some("t20260903T120000Z-abcd".into())
        );
        std::fs::write(
            &summary,
            r#"{"status":"passed","run_id":"t20260903T120000Z-abcd"}"#,
        )
        .expect("terminal summary");
        assert_eq!(active_run_id(temporary.path()).expect("terminal"), None);
        std::fs::remove_file(&summary).expect("remove summary");
        let outside = temporary.path().join("outside.json");
        std::fs::write(&outside, r#"{"status":"running","run_id":"stolen"}"#).expect("outside");
        symlink(&outside, &summary).expect("summary link");
        assert_eq!(
            active_run_id(temporary.path()).expect_err("nofollow"),
            LogQueryError::Unavailable
        );
    }

    #[test]
    fn maintenance_bounds_unsafe_and_missing_worktrees_and_persists_status() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let real = temporary.path().join("real");
        std::fs::create_dir(&real).expect("real");
        let unsafe_path = temporary.path().join("linked");
        symlink(&real, &unsafe_path).expect("link");
        let missing = temporary.path().join("missing");
        let unsafe_text = unsafe_path.display().to_string();
        let missing_text = missing.display().to_string();
        database
            .transaction(move |transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/one','one','t',1,'t'),('r2222222222222222','/two','two','t',1,'t')", [])?;
                transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',$1,'t','t'),('w2222222222222222','r2222222222222222',$2,'t','t')", crate::database::params![unsafe_text,missing_text])?;
                Ok(())
            })
            .expect("fixture");
        let service = TestLogService::with_clock(
            database.clone(),
            Registry::new(database.clone()),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        );
        let report = service.drain_maintenance_for_test().expect("maintenance");
        assert_eq!(report.skipped_missing_worktrees, 1);
        assert_eq!(report.errors.len(), 1);
        assert!(!report.errors_truncated);
        let retention = service.retention().expect("retention");
        assert_eq!(
            retention.last_cleanup_at.as_deref(),
            Some("2026-09-03T12:00:00Z")
        );
        assert_eq!(
            retention.last_cleanup_error_code.as_deref(),
            Some("test_log_unavailable")
        );
    }

    #[test]
    fn native_retention_resumes_bounded_pages_preserves_protection_and_records_expiry() {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repo");
        std::fs::create_dir_all(worktree.join(".devcoordinator/test/logs/runs")).unwrap();
        let database = Database::open(temporary.path().join("postgres-retention-fixture")).unwrap();
        let stored = worktree.to_string_lossy().into_owned();
        database.transaction(|tx|{
            tx.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',$1,'repo','t',1,'t')",[&stored])?;
            tx.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',$1,'t','t')",[&stored])?;
            Ok(())
        }).unwrap();
        let now = datetime!(2026-09-03 12:00 UTC);
        let now_ms = (now.unix_timestamp_nanos() / 1_000_000) as u64;
        let mut runs = Vec::new();
        let mut active_lease = None;
        for sequence in 1..=22 {
            let run_id = format!("t20260902T{sequence:06}Z-abcdef");
            let run_dir = worktree
                .join(".devcoordinator/test/logs/runs")
                .join(&run_id);
            std::fs::create_dir(&run_dir).unwrap();
            let lease = RunLogLease::acquire(&run_dir, &run_id).unwrap();
            let active = sequence == 22;
            let finished = if sequence == 21 {
                now_ms - 172_800_000
            } else {
                now_ms - 60_000 + sequence
            };
            let selector = LeafSelector::case(
                "unit",
                if sequence == 21 {
                    "age-case"
                } else {
                    "shared-case"
                },
            )
            .unwrap();
            let mut stream = lease
                .create_stream(selector.clone(), LogStream::Stdout)
                .unwrap();
            stream.write_all(b"retained private payload\n").unwrap();
            stream.seal().unwrap();
            lease
                .publish_leaf_metadata(&LeafLogMetadata {
                    schema: 2,
                    selector,
                    status: if active {
                        LeafStatus::Running
                    } else {
                        LeafStatus::Passed
                    },
                    exit: DiagnosticExit::default(),
                    started_at_epoch_ms: finished - 100,
                    finished_at_epoch_ms: (!active).then_some(finished),
                    process_started: true,
                    complete: !active,
                    structured_evidence_formats: Vec::new(),
                    structured_evidence_count: 0,
                })
                .unwrap();
            lease
                .publish_run_metadata(&RunLogMetadata {
                    schema: 2,
                    run_id: run_id.clone(),
                    test: "complete".into(),
                    started_at_epoch_ms: finished - 200,
                    finished_at_epoch_ms: (!active).then_some(finished),
                    status: if active {
                        RunStatus::Running
                    } else {
                        RunStatus::Passed
                    },
                    complete: !active,
                })
                .unwrap();
            if active {
                active_lease = Some(lease);
            } else {
                drop(lease);
            }
            runs.push((run_id, run_dir));
        }
        // Ownership follows the same run retention only after exact cleanup
        // is complete. Failed/pending work and a live lease survive its logs.
        let store = crate::test_state::TestRunStore;
        let mut container_leases = Vec::new();
        for (index, status) in [
            (1, "cleanup_complete"),
            (2, "cleanup_pending"),
            (3, "cleanup_failed"),
            (4, "cleanup_complete"),
            (6, "cleanup_complete"),
        ] {
            let run_id = &runs[index].0;
            let directory = store
                .open_container_run(
                    &worktree,
                    run_id,
                    Some((
                        rustix::process::getuid().as_raw(),
                        rustix::process::getgid().as_raw(),
                    )),
                )
                .unwrap()
                .unwrap();
            let mut record: devcoordinator2_api::test_containers::TestContainerRecord = serde_json::from_value(serde_json::json!({
                "container_id":"d".repeat(64),"name":"owned-fixture","run_id":run_id,"check":"postgres",
                "repository_id":"r1111111111111111","worktree_id":"w1111111111111111","owner_token":"fixture-owner",
                "labels":{},"image":"postgres:16-alpine","command":[],"created_at":"2026-09-03T12:00:00Z","cleanup_status":status
            })).unwrap();
            if index == 1 {
                use sha2::{Digest, Sha256};
                let payload = b"owned private container diagnostic\n";
                let reference = store
                    .write_container_diagnostics(
                        &directory,
                        &record.owner_token,
                        payload,
                        rustix::process::getuid().as_raw(),
                        rustix::process::getgid().as_raw(),
                    )
                    .unwrap();
                record.diagnostic =
                    Some(devcoordinator2_api::test_containers::ContainerDiagnostic {
                        reference,
                        bytes: payload.len() as u64,
                        sha256: Sha256::digest(payload)
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                        truncated: false,
                    });
            }
            store
                .write_container_records(
                    &directory,
                    &[record],
                    rustix::process::getuid().as_raw(),
                    rustix::process::getgid().as_raw(),
                )
                .unwrap();
            if index == 4 {
                container_leases.push(
                    store
                        .container_lease(&directory, "fixture-owner", true)
                        .unwrap()
                        .unwrap(),
                );
            }
            if index == 6 {
                container_leases.push(
                    store
                        .container_operation_lease(&directory, "fixture-owner")
                        .unwrap()
                        .unwrap(),
                );
            }
            store
                .archive_container_run(&worktree, run_id, &directory)
                .unwrap();
        }
        {
            let diagnostics = worktree
                .join(".devcoordinator/test/containers/receipts")
                .join(&runs[1].0)
                .join("diagnostics");
            let owned = diagnostics.join("fixture-owner.log");
            let foreign = diagnostics.join("foreign.log");
            std::fs::write(&foreign, b"foreign evidence").unwrap();
            assert!(store.retire_container_run(&worktree, &runs[1].0).is_err());
            assert!(
                owned.exists(),
                "unowned files must prevent destructive retention"
            );
            std::fs::remove_file(&foreign).unwrap();
            std::fs::remove_file(&owned).unwrap();
            std::os::unix::fs::symlink(runs[1].1.join("run.json"), &owned).unwrap();
            assert!(store.retire_container_run(&worktree, &runs[1].0).is_err());
            std::fs::remove_file(&owned).unwrap();
            std::fs::write(&owned, b"owned private container diagnostic\n").unwrap();
        }
        {
            let directory = store
                .open_container_run(
                    &worktree,
                    &runs[5].0,
                    Some((
                        rustix::process::getuid().as_raw(),
                        rustix::process::getgid().as_raw(),
                    )),
                )
                .unwrap()
                .unwrap();
            store
                .archive_container_run(&worktree, &runs[5].0, &directory)
                .unwrap();
        }
        assert!(
            store
                .open_container_run(&worktree, &runs[5].0, None)
                .unwrap()
                .is_none(),
            "a creation refused after allocation must not leave an empty ownership directory"
        );
        // An unfinished feedback task protects an old run without loading the
        // historical feedback or delivery catalog into this maintenance page.
        database.transaction(|tx|{
            tx.execute("INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at) VALUES('p1111111111111111','r1111111111111111',1,1,'feedback','preserve evidence','task','active','t','fixture','t')",[])?;
            tx.execute("INSERT INTO visual_feedback(feedback_id,task_id,repository_id,worktree_id,run_id,check_name,phase,formal_run_id,cell_id,screenshot_kind,screenshot_sha256,image_id,geometry_json,root_comment_id,created_at,created_by,updated_at) VALUES('vf1','p1111111111111111','r1111111111111111','w1111111111111111',$1,'unit','case','formal1','cell1','viewport','digest','image1','{}','comment1','t','fixture','t')",[&runs[0].0])?;
            Ok(())
        }).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(crate::platform::FixedClock(now));
        let mut cursor = None;
        let mut pages = 0;
        let mut removed = 0;
        let mut active = 0;
        loop {
            // Recreate the service and fetch only the durable cursor for each
            // turn, as a process restart does between scheduled pages.
            let service = TestLogService::with_clock(
                database.clone(),
                Registry::new(database.clone()),
                clock.clone(),
            );
            let step = service
                .run_maintenance_step(cursor, 1, Duration::from_millis(100))
                .unwrap();
            assert!(step.visited_entries <= 64);
            assert!(step.processed_bytes <= 256 * 1024);
            assert!(step.report.errors.is_empty(), "{:?}", step.report.errors);
            pages += 1;
            removed += step.report.removed_leaf_folders;
            active += step.report.retained_active;
            if step.complete {
                break;
            }
            assert!(pages < 300, "resumable cleanup must make progress");
            let saved = step.next_worktree_id.unwrap();
            database.maintenance_transaction(|tx|{tx.execute("INSERT INTO coordinator_maintenance(job_key,cursor,due_at_ms,lease_until_ms,attempts,priority,last_status,last_rows,last_bytes) VALUES('test-retention',$1,0,0,0,0,'pending',0,0) ON CONFLICT(job_key) DO UPDATE SET cursor=excluded.cursor",[saved])?;Ok(())}).unwrap();
            cursor = database
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT cursor FROM coordinator_maintenance WHERE job_key='test-retention'",
                        [],
                        |r| r.get::<_, String>(0),
                    )?)
                })
                .unwrap()
                .into();
        }
        assert!(pages > 10);
        assert_eq!(removed, 17);
        assert_eq!(active, 1);
        for (index, (_, directory)) in runs.iter().enumerate() {
            let case = if index == 20 {
                "age-case"
            } else {
                "shared-case"
            };
            assert_eq!(
                directory.join("checks/unit/cases").join(case).exists(),
                index == 0 || (17..=19).contains(&index) || index == 21,
                "run index{index}"
            );
        }
        let (projection, jobs, receipts) = database
            .read(|c| {
                Ok((
                    c.query_row(
                        "SELECT count(*) FROM test_log_retention_projection",
                        [],
                        |r| r.get::<_, u64>(0),
                    )?,
                    c.query_row(
                        "SELECT count(*) FROM storage_jobs WHERE kind='retention'",
                        [],
                        |r| r.get::<_, u64>(0),
                    )?,
                    c.query_row("SELECT count(*) FROM storage_item_receipts", [], |r| {
                        r.get::<_, u64>(0)
                    })?,
                ))
            })
            .unwrap();
        assert_eq!((projection, jobs, receipts), (5, 17, 17));
        assert!(
            store
                .open_container_run(&worktree, &runs[1].0, None)
                .unwrap()
                .is_none()
        );
        for index in [2, 3, 4, 6] {
            assert!(
                store
                    .open_container_run(&worktree, &runs[index].0, None)
                    .unwrap()
                    .is_some(),
                "unfinished ownership for run {index} must survive expired log payloads"
            );
        }
        assert_eq!(
            store.container_runs(&worktree).unwrap().len(),
            4,
            "startup enumerates only pending ownership, not completed history"
        );
        {
            let directory = store
                .open_container_run(&worktree, &runs[2].0, None)
                .unwrap()
                .unwrap();
            let mut records = store.read_container_records(&directory).unwrap();
            records[0].cleanup_status =
                devcoordinator2_api::test_containers::ContainerCleanupStatus::Complete;
            store
                .write_container_records(
                    &directory,
                    &records,
                    rustix::process::getuid().as_raw(),
                    rustix::process::getgid().as_raw(),
                )
                .unwrap();
            store
                .archive_container_run(&worktree, &runs[2].0, &directory)
                .unwrap();
        }
        assert!(
            store
                .open_container_run(&worktree, &runs[2].0, None)
                .unwrap()
                .is_none(),
            "late cleanup must expire even when the old run's metadata directory still exists"
        );
        drop(container_leases);
        crate::storage::evidence::record_run_expired(&database, &worktree, &runs[4].0).unwrap();
        crate::storage::evidence::record_run_expired(&database, &worktree, &runs[6].0).unwrap();
        assert!(
            store
                .open_container_run(&worktree, &runs[6].0, None)
                .unwrap()
                .is_none(),
            "operation-held complete evidence expires only after its operation lease is released"
        );
        assert!(
            store
                .open_container_run(&worktree, &runs[4].0, None)
                .unwrap()
                .is_none()
        );
        let protected = crate::storage::evidence::published_run(&database, &worktree, &runs[0].0)
            .unwrap()
            .unwrap();
        assert_eq!(protected.payload_leaves, 1);
        assert!(protected.eligible);
        assert!(!protected.active);
        let active_state =
            crate::storage::evidence::published_run(&database, &worktree, &runs[21].0)
                .unwrap()
                .unwrap();
        assert!(active_state.active && !active_state.eligible);
        assert_eq!(
            crate::storage::evidence::published_run(&database, &worktree, &runs[1].0)
                .unwrap()
                .unwrap()
                .payload_leaves,
            0
        );
        assert!(
            crate::storage::evidence::published_run_observation(&database, &worktree, &runs[0].0)
                .unwrap()
                .is_some()
        );
        assert!(
            crate::storage::evidence::published_run_observation(&database, &worktree, &runs[1].0)
                .unwrap()
                .is_none()
        );
        assert!(
            crate::storage::evidence::run_retention_state(
                &database, &worktree, &runs[0].0, 86401, 3
            )
            .unwrap()
            .is_none()
        );
        assert!(
            TestLogService::with_clock(database.clone(), Registry::new(database), clock)
                .retention()
                .unwrap()
                .last_cleanup_at
                .is_some()
        );
        drop(active_lease);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn maintenance_loop_wakes_and_stops_event_driven() {
        use crate::automation_test_support::{Fixture, FixtureClock};
        use crate::maintenance::{Scheduler, jobs};
        use tokio::sync::watch;
        let fixture = Fixture::new();
        let plane = tokio::task::spawn_blocking({
            let config = fixture.config.clone();
            let database = fixture.database.clone();
            move || {
                crate::control_plane::ControlPlane::with_adapters(
                    config,
                    database,
                    Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
                    Arc::new(FixtureClock),
                )
            }
        })
        .await
        .unwrap()
        .unwrap();
        let scheduler = Scheduler::new(fixture.database.clone(), 1, 8).unwrap();
        let mut log_job = jobs::periodic(&plane)
            .into_iter()
            .find(|spec| spec.job.name() == "log-retention")
            .unwrap();
        log_job.interval = Duration::from_secs(3600);
        let (stop, receiver) = watch::channel(false);
        let serving = scheduler.clone();
        let serving_job = log_job.clone();
        let task = tokio::spawn(async move { serving.serve(vec![serving_job], receiver).await });
        async fn completed(database: &Database, after: u64) -> u64 {
            tokio::time::timeout(Duration::from_secs(10),async {
                loop {
                    let attempt=database.read(|c|c.query_row("SELECT attempts FROM coordinator_maintenance WHERE job_key='log-retention' AND last_status='complete' AND attempts>$1",[after],|row|row.get::<_,u64>(0)).optional().map_err(DatabaseError::from)).unwrap();
                    if let Some(attempt)=attempt { return attempt; }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.expect("shared scheduler must complete real log maintenance")
        }
        let initial = completed(&fixture.database, 0).await;
        plane
            .logs()
            .set_retention(1234, 7, "fixture", "1970-01-08T00:16:40Z")
            .unwrap();
        let changed = completed(&fixture.database, initial).await;
        assert!(
            changed > initial,
            "public cleanup notification must bypass the one-hour periodic deadline"
        );
        assert!(plane.logs().retention().unwrap().last_cleanup_at.is_some());
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shared scheduler stopped")
            .unwrap();
        tokio::task::spawn_blocking(move || scheduler.shutdown_and_join())
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || {
            drop((plane, fixture, log_job));
        })
        .await
        .unwrap();
    }

    #[test]
    fn public_queries_resolve_only_an_exact_registered_worktree() {
        let temporary = tempdir().expect("tempdir");
        let worktree = temporary.path().join("repo");
        std::fs::create_dir(&worktree).expect("worktree");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let stored = worktree.display().to_string();
        database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',$1,'repo','t',1,'t')",
                    [&stored],
                )?;
                transaction.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',$1,'t','t')",
                    [&stored],
                )?;
                Ok(())
            })
            .expect("fixture");
        let service = TestLogService::new(database.clone(), Registry::new(database));
        let caller = Caller {
            via_edge: false,
            pid: 1,
            uid: 999,
            gid: 999,
            client_kind: devcoordinator2_api::ClientKind::Edge,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: Some("reader@example.test".into()),
        };
        let error = service
            .query(
                LogQueryOperation::Catalog,
                &worktree,
                serde_json::json!({"path":worktree,"limit":100}),
                &caller,
            )
            .expect_err("empty retained store");
        assert_eq!(error.code, ErrorCode::LogNotFound);

        let nested = worktree.join("nested");
        let denied = service
            .query(
                LogQueryOperation::Catalog,
                &nested,
                serde_json::json!({"path":nested,"limit":100}),
                &caller,
            )
            .expect_err("public path must be exact");
        assert_eq!(denied.code, ErrorCode::RepositoryNotFound);
    }
}
