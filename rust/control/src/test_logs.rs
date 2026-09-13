//! Typed in-process access to the Rust governed-log store.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use devcoordinator2_api::results::{Retention, RetentionDefaults};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use devcoordinator2_executor_core::log_query::{
    LogPruneRequest, LogQueryError, LogQueryOperation, LogQueryOptions, LogQueryRequest,
    LogQueryResult, LogQuerySelector, execute_log_query, prune_logs,
};
use rusqlite::OptionalExtension;
use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
use serde_json::{Map, Value};
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, sleep_until};

use crate::access::Caller;
use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;

pub const DEFAULT_MAX_AGE_SECONDS: u32 = 86_400;
pub const DEFAULT_CASE_DEPTH: u16 = 3;
const SUMMARY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MAINTENANCE_ERRORS: usize = 64;
const ERROR_RETRY: Duration = Duration::from_secs(60);
const MINIMUM_DEADLINE: Duration = Duration::from_millis(100);
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceError {
    pub repository_id: String,
    pub code: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceReport {
    pub removed_leaf_folders: u64,
    pub retained_active: u64,
    pub next_expiry_at: Option<String>,
    pub skipped_missing_worktrees: u64,
    pub errors: Vec<MaintenanceError>,
    pub errors_truncated: bool,
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
            .call(|connection| {
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
                        "UPDATE test_log_retention_state SET max_age_seconds=?1,case_depth=?2,updated_at=?3,updated_by=?4 WHERE singleton=1",
                        rusqlite::params![max_age_seconds, case_depth, now, actor],
                    )?;
                    transaction.execute(
                        "INSERT INTO test_log_retention_events(at,actor,previous_max_age_seconds,max_age_seconds,previous_case_depth,case_depth) VALUES(?1,?2,?3,?4,?5,?6)",
                        rusqlite::params![
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

    pub fn notify_run_finished(&self, _worktree: &Path, _run_id: &str) {
        self.request_cleanup();
    }

    pub fn run_maintenance_once(&self) -> Result<MaintenanceReport, ProtocolError> {
        let settings = self.retention()?;
        let worktrees = self
            .database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT worktree_path,repository_id FROM worktrees ORDER BY worktree_id",
                )?;
                Ok(statement
                    .query_map([], |row| {
                        Ok((
                            PathBuf::from(row.get::<_, String>(0)?),
                            row.get::<_, String>(1)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        let mut removed_leaf_folders = 0_u64;
        let mut retained_active = 0_u64;
        let mut next_expiry_at: Option<String> = None;
        let mut skipped_missing_worktrees = 0_u64;
        let mut errors = Vec::new();
        let mut error_count = 0_usize;
        for (worktree, repository_id) in worktrees {
            let metadata = match std::fs::symlink_metadata(&worktree) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    skipped_missing_worktrees = skipped_missing_worktrees.saturating_add(1);
                    continue;
                }
                Err(_) => {
                    push_maintenance_error(&mut errors, &mut error_count, repository_id);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                push_maintenance_error(&mut errors, &mut error_count, repository_id);
                continue;
            }
            let active_run_id = match active_run_id(&worktree) {
                Ok(active) => active,
                Err(_) => {
                    push_maintenance_error(&mut errors, &mut error_count, repository_id);
                    continue;
                }
            };
            match prune_logs(
                &worktree,
                LogPruneRequest {
                    schema: 2,
                    repository_id: repository_id.clone(),
                    max_age_seconds: u64::from(settings.max_age_seconds),
                    case_depth: u64::from(settings.case_depth),
                    active_run_id,
                },
            ) {
                Ok(result) => {
                    removed_leaf_folders =
                        removed_leaf_folders.saturating_add(result.removed_leaf_folders);
                    retained_active = retained_active.saturating_add(result.retained_active);
                    if let Some(candidate) = result.next_expiry_at
                        && next_expiry_at
                            .as_ref()
                            .is_none_or(|current| candidate < *current)
                    {
                        next_expiry_at = Some(candidate);
                    }
                }
                Err(_) => push_maintenance_error(&mut errors, &mut error_count, repository_id),
            }
        }
        let last_cleanup_at = timestamp(self.clock.as_ref())?;
        let last_cleanup_error_code = errors.first().map(|error| error.code.clone());
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE test_log_retention_state SET last_cleanup_at=?1,last_cleanup_error_code=?2 WHERE singleton=1",
                    rusqlite::params![last_cleanup_at, last_cleanup_error_code],
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        Ok(MaintenanceReport {
            removed_leaf_folders,
            retained_active,
            next_expiry_at,
            skipped_missing_worktrees,
            errors,
            errors_truncated: error_count > MAX_MAINTENANCE_ERRORS,
        })
    }

    pub async fn serve_maintenance(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let service = self.clone();
            let report = tokio::task::spawn_blocking(move || service.run_maintenance_once()).await;
            let delay = match report {
                Ok(Ok(report)) if !report.errors.is_empty() => Some(ERROR_RETRY),
                Ok(Ok(report)) => report
                    .next_expiry_at
                    .as_deref()
                    .and_then(|expiry| delay_until(expiry, self.clock.as_ref())),
                Ok(Err(_)) | Err(_) => Some(ERROR_RETRY),
            };
            match delay {
                Some(delay) => {
                    let deadline = Instant::now() + delay;
                    tokio::select! {
                        _ = self.maintenance_wake.notified() => {}
                        _ = sleep_until(deadline) => {}
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                    }
                }
                None => {
                    tokio::select! {
                        _ = self.maintenance_wake.notified() => {}
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                return;
                            }
                        }
                    }
                }
            }
        }
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
        if caller.identity.is_some() {
            let path = path.to_string_lossy().to_string();
            return self
                .database
                .call(move |connection| {
                    connection
                        .query_row(
                            "SELECT worktree_path,repository_id FROM worktrees WHERE worktree_path=?1",
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

fn active_run_id(worktree: &Path) -> Result<Option<String>, LogQueryError> {
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

fn delay_until(expiry: &str, clock: &dyn Clock) -> Option<Duration> {
    let expiry = OffsetDateTime::parse(expiry, TIMESTAMP_FORMAT).ok()?;
    let remaining = expiry - clock.now_utc();
    if remaining.is_negative() || remaining.is_zero() {
        return Some(MINIMUM_DEADLINE);
    }
    Duration::try_from(remaining)
        .ok()
        .map(|duration| duration.max(MINIMUM_DEADLINE))
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
            .call(|connection| {
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
                transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',?1,'t','t'),('w2222222222222222','r2222222222222222',?2,'t','t')", rusqlite::params![unsafe_text,missing_text])?;
                Ok(())
            })
            .expect("fixture");
        let service = TestLogService::with_clock(
            database.clone(),
            Registry::new(database.clone()),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        );
        let report = service.run_maintenance_once().expect("maintenance");
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

    #[tokio::test]
    async fn maintenance_loop_wakes_and_stops_event_driven() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let service = TestLogService::new(database.clone(), Registry::new(database));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let running = service.clone();
        let task = tokio::spawn(async move { running.serve_maintenance(shutdown_rx).await });
        service.request_cleanup();
        tokio::task::yield_now().await;
        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("maintenance stopped")
            .expect("task");
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
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',?1,'repo','t',1,'t')",
                    [&stored],
                )?;
                transaction.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',?1,'t','t')",
                    [&stored],
                )?;
                Ok(())
            })
            .expect("fixture");
        let service = TestLogService::new(database.clone(), Registry::new(database));
        let caller = Caller {
            pid: 1,
            uid: 999,
            gid: 999,
            client_kind: devcoordinator2_api::ClientKind::Edge,
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
