//! Bounded, no-follow queries and exact retention cleanup for governed logs.
//!
//! Complete output remains in the repository-local log store. This module
//! returns only content-free catalogue rows or explicitly requested bounded
//! slices selected by logical run/check/phase/case/stream identities.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use devcoordinator2_executor_protocol::{
    DiagnosticReportFormat, FailureIndexEntry, LogPhase, LogRef, LogStream, MAX_DIAGNOSTIC_EVENTS,
};
use rustix::fs::{self as unix_fs, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::diagnostics::{diagnostic_fingerprint, diagnostic_rank};
use crate::log_store::{LeafLogMetadata, LeafSelector, RunLogMetadata, StreamMetadata};
use crate::retention::{RetentionEntry, RetentionPolicy, select_expired};

pub const MAX_QUERY_CONTENT_BYTES: usize = 48 * 1024;
pub const MAX_QUERY_RESULT_BYTES: usize = 60 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024;
const MAX_BASE64_SOURCE_BYTES: usize = 32 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const MAX_DIAGNOSTICS_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_CATALOG_ROWS: usize = 100;
const MAX_MATCHES: usize = 100;
const MAX_CONTEXT_LINES: u64 = 100;
const MAX_SEARCH_BYTES: usize = 4096;
const READ_BLOCK_BYTES: usize = 64 * 1024;
const CURSOR_DOMAIN: &[u8] = b"devcoordinator2-log-cursor-v1\0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogQueryOperation {
    Catalog,
    Tail,
    Search,
    Range,
    FailureContext,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogQuerySelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<LogPhase>,
    #[serde(default, rename = "case", skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<LogStream>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogQueryOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_end: Option<u64>,
    // The daemon will populate these for catalogue expiry/depth metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_depth: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogQueryRequest {
    pub schema: u8,
    pub operation: LogQueryOperation,
    pub repository_id: String,
    pub selector: LogQuerySelector,
    pub options: LogQueryOptions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogPruneRequest {
    pub schema: u8,
    pub repository_id: String,
    pub max_age_seconds: u64,
    pub case_depth: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredEvidenceSummary {
    pub available: bool,
    pub formats: Vec<DiagnosticReportFormat>,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalogEntry {
    pub log_ref: LogRef,
    pub bytes: u64,
    pub lines: Option<u64>,
    pub first_byte_at: Option<String>,
    pub last_byte_at: Option<String>,
    pub complete: bool,
    pub truncated: bool,
    pub sha256: Option<String>,
    pub expires_at: Option<String>,
    pub depth_rank: Option<u64>,
    pub structured_evidence: StructuredEvidenceSummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogSegment {
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogResult {
    pub entries: Vec<LogCatalogEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentResult {
    pub segments: Vec<LogSegment>,
    pub snapshot_bytes: u64,
    pub snapshot_lines: u64,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResult {
    pub matches: Vec<LogSegment>,
    pub snapshot_bytes: u64,
    pub snapshot_lines: u64,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailureContextResult {
    pub failures: Vec<FailureIndexEntry>,
    pub contexts: Vec<LogSegment>,
    pub snapshot_bytes: Option<u64>,
    pub snapshot_lines: Option<u64>,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LogQueryResult {
    Catalog(CatalogResult),
    Content(ContentResult),
    Search(SearchResult),
    FailureContext(FailureContextResult),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogPruneResult {
    pub removed_leaf_folders: u64,
    pub retained_active: u64,
    pub next_expiry_at: Option<String>,
    pub recovered_garbage: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogQueryError {
    ArgsInvalid,
    LogNotFound,
    LogExpired,
    CursorStale,
    StoreMalformed,
    Unavailable,
    MaintenanceBusy,
}

impl LogQueryError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ArgsInvalid => "args_invalid",
            Self::LogNotFound => "log_not_found",
            Self::LogExpired => "log_expired",
            Self::CursorStale => "cursor_stale",
            Self::StoreMalformed | Self::Unavailable | Self::MaintenanceBusy => {
                "test_log_unavailable"
            }
        }
    }
}

impl fmt::Display for LogQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for LogQueryError {}

#[derive(Clone)]
struct InventoryLeaf {
    run_id: String,
    test: String,
    selector: LeafSelector,
    metadata: LeafLogMetadata,
    relative_components: Vec<String>,
    directory_identity: (u64, u64),
    active: bool,
    streams: BTreeMap<LogStream, StreamMetadata>,
}

struct InventoryRun {
    metadata: RunLogMetadata,
    active: bool,
    leaves: Vec<InventoryLeaf>,
    directory: Option<File>,
    directory_identity: (u64, u64),
    executor_streams: BTreeMap<LogStream, UnindexedStream>,
}

#[derive(Clone)]
struct UnindexedStream {
    bytes: u64,
    lines: u64,
    device: u64,
    inode: u64,
    sha256: String,
}

struct StoreInventory {
    logs_dir: File,
    runs_dir: File,
    runs: Vec<InventoryRun>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDiagnostics {
    schema: u8,
    check: Option<String>,
    #[serde(rename = "case")]
    case_id: Option<String>,
    entries: Vec<FailureIndexEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorPayload {
    schema: u8,
    operation: LogQueryOperation,
    log_ref: Option<LogRef>,
    run_id: String,
    device: u64,
    inode: u64,
    snapshot_bytes: u64,
    snapshot_lines: u64,
    position: u64,
    byte_position: u64,
    auxiliary_position: u64,
    auxiliary_byte_position: u64,
    pending_line_end: Option<u64>,
    emitted_through_line: u64,
    query_sha256: String,
}

#[derive(Clone)]
struct RetentionLocation {
    run_id: String,
    components: Vec<String>,
    run_identity: (u64, u64),
    leaf_identity: (u64, u64),
}

/// Execute one content-free catalogue or explicit bounded log query.
pub fn execute_log_query(
    worktree: &Path,
    request: LogQueryRequest,
) -> Result<LogQueryResult, LogQueryError> {
    validate_query_request(&request)?;
    let explicit_run = request.selector.run_id.is_some();
    let run_id = match request.selector.run_id.as_deref() {
        Some(run_id) => run_id.to_owned(),
        None => current_run_id(worktree)?,
    };
    validate_run_id(&run_id)?;
    let inventory = if request.operation == LogQueryOperation::Catalog {
        scan_store(worktree, None, Some(&run_id))?
    } else {
        let root = open_directory_path(worktree)?;
        let devcoordinator = open_dir(&root, ".devcoordinator")?;
        let test = open_dir(&devcoordinator, "test")?;
        let logs_dir = open_dir(&test, "logs")?;
        let runs_dir = open_dir(&logs_dir, "runs")?;
        let run = scan_inventory_run(&runs_dir, &run_id, None, true)?;
        StoreInventory {
            logs_dir,
            runs_dir,
            runs: vec![run],
        }
    };
    let run = inventory
        .runs
        .iter()
        .find(|run| run.metadata.run_id == run_id)
        .ok_or(if explicit_run {
            LogQueryError::LogNotFound
        } else {
            LogQueryError::LogExpired
        })?;
    let result = match request.operation {
        LogQueryOperation::Catalog => {
            LogQueryResult::Catalog(query_catalog(&inventory, run, &request)?)
        }
        LogQueryOperation::Tail => LogQueryResult::Content(query_tail(run, &request)?),
        LogQueryOperation::Search => LogQueryResult::Search(query_search(run, &request)?),
        LogQueryOperation::Range => LogQueryResult::Content(query_range(run, &request)?),
        LogQueryOperation::FailureContext => {
            LogQueryResult::FailureContext(query_failure_context(run, &request)?)
        }
    };
    if serde_json::to_vec(&result)
        .map_err(|_| LogQueryError::Unavailable)?
        .len()
        > MAX_QUERY_RESULT_BYTES
    {
        return Err(LogQueryError::Unavailable);
    }
    Ok(result)
}

/// Apply one age-or-depth retention pass. Selection completes before mutation.
pub fn prune_logs(
    worktree: &Path,
    request: LogPruneRequest,
) -> Result<LogPruneResult, LogQueryError> {
    validate_prune_request(&request)?;
    let inventory = match scan_store(worktree, request.active_run_id.as_deref(), None) {
        Ok(inventory) => inventory,
        Err(LogQueryError::LogNotFound) => {
            return Ok(LogPruneResult {
                removed_leaf_folders: 0,
                retained_active: 0,
                next_expiry_at: None,
                recovered_garbage: 0,
            });
        }
        Err(error) => return Err(error),
    };
    let maintenance = open_or_create_file(&inventory.logs_dir, "maintenance.lock")?;
    match unix_fs::flock(&maintenance, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error)
            if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::WOULDBLOCK =>
        {
            return Err(LogQueryError::MaintenanceBusy);
        }
        Err(_) => return Err(LogQueryError::Unavailable),
    }
    let now_ms = epoch_ms();
    let policy = RetentionPolicy {
        max_age_seconds: request.max_age_seconds,
        history_depth: usize::try_from(request.case_depth)
            .map_err(|_| LogQueryError::ArgsInvalid)?,
    };
    let mut entries = Vec::new();
    let mut locations = BTreeMap::<PathBuf, RetentionLocation>::new();
    for run in &inventory.runs {
        let run_directory = open_dir(&inventory.runs_dir, &run.metadata.run_id)?;
        let run_identity = file_identity(&run_directory)?;
        if run_identity != run.directory_identity {
            return Err(LogQueryError::StoreMalformed);
        }
        if let Some(finished) = run.metadata.finished_at_epoch_ms
            && finished > now_ms
        {
            return Err(LogQueryError::StoreMalformed);
        }
        if !run.executor_streams.is_empty() {
            let finished_at_ms = match run.metadata.finished_at_epoch_ms {
                Some(value) => value,
                None if run.active => 0,
                None => run.metadata.started_at_epoch_ms,
            };
            let components = vec![run.metadata.run_id.clone(), "executor".into()];
            let directory = relative_leaf_path(&run.metadata.run_id, &components);
            let executor = required_store_entry(open_dir(&run_directory, "executor"))?;
            locations.insert(
                directory.clone(),
                RetentionLocation {
                    run_id: run.metadata.run_id.clone(),
                    components,
                    run_identity,
                    leaf_identity: file_identity(&executor)?,
                },
            );
            entries.push(RetentionEntry {
                run_id: run.metadata.run_id.clone(),
                test: retention_test_key(&run.metadata.test, run.metadata.complete),
                check: None,
                phase: LogPhase::Executor,
                case: None,
                directory,
                finished_at_ms,
                active: run.active,
            });
        }
        for leaf in &run.leaves {
            let finished_at_ms = match leaf.metadata.finished_at_epoch_ms {
                Some(value) => value,
                None if leaf.active => 0,
                None => run
                    .metadata
                    .finished_at_epoch_ms
                    .unwrap_or(leaf.metadata.started_at_epoch_ms),
            };
            if finished_at_ms > now_ms {
                return Err(LogQueryError::StoreMalformed);
            }
            let directory = relative_leaf_path(&leaf.run_id, &leaf.relative_components);
            locations.insert(
                directory.clone(),
                RetentionLocation {
                    run_id: leaf.run_id.clone(),
                    components: leaf.relative_components.clone(),
                    run_identity,
                    leaf_identity: leaf.directory_identity,
                },
            );
            entries.push(RetentionEntry {
                run_id: leaf.run_id.clone(),
                test: retention_test_key(&leaf.test, leaf.metadata.complete),
                check: leaf.selector.check.clone(),
                phase: leaf.selector.phase,
                case: leaf.selector.case_id.clone(),
                directory,
                finished_at_ms,
                active: leaf.active,
            });
        }
    }
    let decision =
        select_expired(&entries, now_ms, policy).map_err(|_| LogQueryError::ArgsInvalid)?;
    let garbage = ensure_dir(&inventory.logs_dir, ".garbage")?;
    let recovered_garbage = recover_garbage(&garbage)?;
    let mut removed = 0_u64;
    let mut retained_active = u64::try_from(decision.retained_active).unwrap_or(u64::MAX);
    for victim in &decision.victims {
        let location = locations.get(victim).ok_or(LogQueryError::StoreMalformed)?;
        let Some(_run_guard) = lock_and_revalidate_victim(
            &inventory.runs_dir,
            location,
            request.active_run_id.as_deref(),
        )?
        else {
            retained_active = retained_active.saturating_add(1);
            continue;
        };
        rename_leaf_to_garbage(
            &inventory.runs_dir,
            &garbage,
            &location.components,
            location.leaf_identity,
        )?;
        removed = removed.saturating_add(1);
    }
    Ok(LogPruneResult {
        removed_leaf_folders: removed,
        retained_active,
        next_expiry_at: decision.next_age_expiry_ms.map(iso_from_epoch_ms),
        recovered_garbage,
    })
}

fn validate_query_request(request: &LogQueryRequest) -> Result<(), LogQueryError> {
    if request.schema != 2 || !valid_repository_id(&request.repository_id) {
        return Err(LogQueryError::ArgsInvalid);
    }
    if let Some(run_id) = request.selector.run_id.as_deref() {
        validate_run_id(run_id)?;
    }
    validate_selector_filter(&request.selector, request.operation)?;
    if request.options.cursor.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > MAX_CURSOR_BYTES || !value.is_ascii()
    }) {
        return Err(LogQueryError::ArgsInvalid);
    }
    let options = &request.options;
    match request.operation {
        LogQueryOperation::Catalog => {
            bounded_positive(options.limit.unwrap_or(100), MAX_CATALOG_ROWS as u64)?;
            reject_options(
                options,
                &["cursor", "limit", "max_age_seconds", "case_depth"],
            )?;
            if let Some(age) = options.max_age_seconds {
                bounded_positive(age, u64::MAX)?;
            }
            if let Some(depth) = options.case_depth {
                bounded_positive(depth, u64::MAX)?;
            }
        }
        LogQueryOperation::Tail => {
            bounded_positive(options.lines.unwrap_or(50), 5_000)?;
            validate_max_bytes(options.max_bytes.unwrap_or(32_768))?;
            reject_options(options, &["cursor", "lines", "max_bytes"])?;
        }
        LogQueryOperation::Search => {
            let text = options.text.as_deref().ok_or(LogQueryError::ArgsInvalid)?;
            if text.is_empty() || text.len() > MAX_SEARCH_BYTES {
                return Err(LogQueryError::ArgsInvalid);
            }
            bounded_positive(options.max_matches.unwrap_or(20), MAX_MATCHES as u64)?;
            if options.context_lines.unwrap_or(2) > MAX_CONTEXT_LINES {
                return Err(LogQueryError::ArgsInvalid);
            }
            validate_max_bytes(options.max_bytes.unwrap_or(32_768))?;
            reject_options(
                options,
                &[
                    "cursor",
                    "text",
                    "max_matches",
                    "context_lines",
                    "max_bytes",
                ],
            )?;
        }
        LogQueryOperation::Range => {
            validate_max_bytes(options.max_bytes.unwrap_or(MAX_QUERY_CONTENT_BYTES as u64))?;
            let lines = (options.line_start, options.line_end);
            let bytes = (options.byte_start, options.byte_end);
            let line_mode = lines.0.is_some() || lines.1.is_some();
            let byte_mode = bytes.0.is_some() || bytes.1.is_some();
            if line_mode == byte_mode {
                return Err(LogQueryError::ArgsInvalid);
            }
            if line_mode {
                let (Some(start), Some(end)) = lines else {
                    return Err(LogQueryError::ArgsInvalid);
                };
                if start == 0 || end < start {
                    return Err(LogQueryError::ArgsInvalid);
                }
            } else {
                let (Some(start), Some(end)) = bytes else {
                    return Err(LogQueryError::ArgsInvalid);
                };
                if end < start {
                    return Err(LogQueryError::ArgsInvalid);
                }
            }
            reject_options(
                options,
                &[
                    "cursor",
                    "max_bytes",
                    "line_start",
                    "line_end",
                    "byte_start",
                    "byte_end",
                ],
            )?;
        }
        LogQueryOperation::FailureContext => {
            bounded_positive(options.limit.unwrap_or(20), MAX_MATCHES as u64)?;
            if options.context_lines.unwrap_or(2) > MAX_CONTEXT_LINES {
                return Err(LogQueryError::ArgsInvalid);
            }
            validate_max_bytes(options.max_bytes.unwrap_or(32_768))?;
            reject_options(options, &["cursor", "limit", "context_lines", "max_bytes"])?;
            if request.selector.phase.is_none() || request.selector.stream.is_none() {
                return Err(LogQueryError::ArgsInvalid);
            }
        }
    }
    Ok(())
}

fn reject_options(options: &LogQueryOptions, allowed: &[&str]) -> Result<(), LogQueryError> {
    let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
    let present = [
        ("cursor", options.cursor.is_some()),
        ("limit", options.limit.is_some()),
        ("lines", options.lines.is_some()),
        ("max_bytes", options.max_bytes.is_some()),
        ("text", options.text.is_some()),
        ("max_matches", options.max_matches.is_some()),
        ("context_lines", options.context_lines.is_some()),
        ("line_start", options.line_start.is_some()),
        ("line_end", options.line_end.is_some()),
        ("byte_start", options.byte_start.is_some()),
        ("byte_end", options.byte_end.is_some()),
        ("max_age_seconds", options.max_age_seconds.is_some()),
        ("case_depth", options.case_depth.is_some()),
    ];
    if present
        .into_iter()
        .any(|(name, present)| present && !allowed.contains(name))
    {
        return Err(LogQueryError::ArgsInvalid);
    }
    Ok(())
}

fn validate_prune_request(request: &LogPruneRequest) -> Result<(), LogQueryError> {
    if request.schema != 2
        || !valid_repository_id(&request.repository_id)
        || request.max_age_seconds == 0
        || request.case_depth == 0
        || usize::try_from(request.case_depth).is_err()
    {
        return Err(LogQueryError::ArgsInvalid);
    }
    if let Some(run_id) = request.active_run_id.as_deref() {
        validate_run_id(run_id)?;
    }
    Ok(())
}

fn validate_selector_filter(
    selector: &LogQuerySelector,
    operation: LogQueryOperation,
) -> Result<(), LogQueryError> {
    if let Some(check) = selector.check.as_deref()
        && !valid_check(check)
    {
        return Err(LogQueryError::ArgsInvalid);
    }
    if let Some(case_id) = selector.case_id.as_deref()
        && !valid_case(case_id)
    {
        return Err(LogQueryError::ArgsInvalid);
    }
    match selector.phase {
        Some(LogPhase::Executor) if selector.check.is_some() || selector.case_id.is_some() => {
            return Err(LogQueryError::ArgsInvalid);
        }
        Some(LogPhase::Check | LogPhase::Discovery)
            if selector.check.is_none() || selector.case_id.is_some() =>
        {
            return Err(LogQueryError::ArgsInvalid);
        }
        Some(LogPhase::Case | LogPhase::Fixture | LogPhase::Cleanup)
            if selector.check.is_none()
                || (selector.case_id.is_none() && operation != LogQueryOperation::Catalog) =>
        {
            return Err(LogQueryError::ArgsInvalid);
        }
        None if selector.case_id.is_some() => return Err(LogQueryError::ArgsInvalid),
        _ => {}
    }
    if matches!(
        operation,
        LogQueryOperation::Tail | LogQueryOperation::Search | LogQueryOperation::Range
    ) && (selector.phase.is_none() || selector.stream.is_none())
    {
        return Err(LogQueryError::ArgsInvalid);
    }
    Ok(())
}

fn bounded_positive(value: u64, maximum: u64) -> Result<u64, LogQueryError> {
    if value == 0 || value > maximum {
        Err(LogQueryError::ArgsInvalid)
    } else {
        Ok(value)
    }
}

fn validate_max_bytes(value: u64) -> Result<usize, LogQueryError> {
    if value == 0 || value > MAX_QUERY_CONTENT_BYTES as u64 {
        return Err(LogQueryError::ArgsInvalid);
    }
    usize::try_from(value).map_err(|_| LogQueryError::ArgsInvalid)
}

fn scan_store(
    worktree: &Path,
    active_run_id: Option<&str>,
    selected_run_id: Option<&str>,
) -> Result<StoreInventory, LogQueryError> {
    let root = open_directory_path(worktree)?;
    let devcoordinator = open_dir(&root, ".devcoordinator")?;
    let test = open_dir(&devcoordinator, "test")?;
    let logs_dir = open_dir(&test, "logs")?;
    for name in directory_names(&logs_dir)? {
        if !matches!(name.as_str(), "runs" | ".garbage" | "maintenance.lock") {
            return Err(LogQueryError::StoreMalformed);
        }
    }
    let runs_dir = required_store_entry(open_dir(&logs_dir, "runs"))?;
    let mut runs = Vec::new();
    for run_id in directory_names(&runs_dir)? {
        if validate_run_id(&run_id).is_err() {
            if selected_run_id.is_some() {
                continue;
            }
            return Err(LogQueryError::StoreMalformed);
        }
        match scan_inventory_run(
            &runs_dir,
            &run_id,
            active_run_id,
            selected_run_id.is_none_or(|id| id == run_id),
        ) {
            Ok(mut run) => {
                if selected_run_id != Some(run_id.as_str()) {
                    run.directory = None;
                }
                runs.push(run);
            }
            // An exact query must fail closed for its selected run, but an
            // unrelated malformed or half-published run cannot deny access to
            // otherwise valid evidence. Retention passes remain store-strict.
            Err(_) if selected_run_id.is_some_and(|selected| selected != run_id) => continue,
            Err(error) => return Err(error),
        }
    }
    runs.sort_by(|left, right| left.metadata.run_id.cmp(&right.metadata.run_id));
    Ok(StoreInventory {
        logs_dir,
        runs_dir,
        runs,
    })
}

fn scan_inventory_run(
    runs_dir: &File,
    run_id: &str,
    active_run_id: Option<&str>,
    verify_streams: bool,
) -> Result<InventoryRun, LogQueryError> {
    let run_dir = required_store_entry(open_dir(runs_dir, run_id))?;
    let metadata: RunLogMetadata =
        required_store_entry(read_json(&run_dir, "run.json", MAX_METADATA_BYTES))?;
    metadata
        .validate()
        .map_err(|_| LogQueryError::StoreMalformed)?;
    if metadata.run_id != run_id {
        return Err(LogQueryError::StoreMalformed);
    }
    let pending =
        unix_fs::statat(&run_dir, "finalization.pending", AtFlags::SYMLINK_NOFOLLOW).is_ok();
    let active =
        active_run_id == Some(run_id) || pending || required_store_entry(run_is_locked(&run_dir))?;
    let executor_streams = if verify_streams {
        scan_executor_streams(&run_dir)?
    } else {
        BTreeMap::new()
    };
    // A valid incomplete run can be crash residue after its lease is gone.
    // Missing unsealed metadata is recoverable from a bounded file snapshot;
    // present-but-invalid metadata and unsafe filesystem objects still fail.
    let leaves = scan_run_leaves(
        &run_dir,
        &metadata,
        active,
        active || !metadata.complete,
        verify_streams,
    )?;
    Ok(InventoryRun {
        metadata,
        active,
        leaves,
        directory_identity: file_identity(&run_dir)?,
        directory: Some(run_dir),
        executor_streams,
    })
}

fn required_store_entry<T>(result: Result<T, LogQueryError>) -> Result<T, LogQueryError> {
    result.map_err(|error| {
        if error == LogQueryError::LogNotFound {
            LogQueryError::StoreMalformed
        } else {
            error
        }
    })
}

fn scan_run_leaves(
    run_dir: &File,
    run: &RunLogMetadata,
    active: bool,
    allow_unsealed: bool,
    verify_streams: bool,
) -> Result<Vec<InventoryLeaf>, LogQueryError> {
    let names = directory_names(run_dir)?;
    if names.iter().any(|name| {
        !matches!(
            name.as_str(),
            "active.lock" | "run.json" | "executor" | "checks" | "finalization.pending"
        )
    }) {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut leaves = Vec::new();
    if names.iter().any(|name| name == "checks") {
        let checks = required_store_entry(open_dir(run_dir, "checks"))?;
        for check_name in directory_names(&checks)? {
            if !valid_check(&check_name) {
                return Err(LogQueryError::StoreMalformed);
            }
            let check_dir = required_store_entry(open_dir(&checks, &check_name))?;
            for child in directory_names(&check_dir)? {
                match child.as_str() {
                    "check" | "discovery" => {
                        let phase = if child == "check" {
                            LogPhase::Check
                        } else {
                            LogPhase::Discovery
                        };
                        let directory = required_store_entry(open_dir(&check_dir, &child))?;
                        scan_leaf(
                            &directory,
                            run,
                            LeafSelector::new(Some(check_name.clone()), phase, None)
                                .map_err(|_| LogQueryError::StoreMalformed)?,
                            vec![
                                run.run_id.clone(),
                                "checks".into(),
                                check_name.clone(),
                                child,
                            ],
                            active,
                            allow_unsealed,
                            verify_streams,
                            &mut leaves,
                        )?;
                    }
                    "cases" | "fixtures" | "cleanup" => {
                        let phase = match child.as_str() {
                            "fixtures" => LogPhase::Fixture,
                            "cleanup" => LogPhase::Cleanup,
                            _ => LogPhase::Case,
                        };
                        let cases = required_store_entry(open_dir(&check_dir, &child))?;
                        for case_id in directory_names(&cases)? {
                            if !valid_case(&case_id) {
                                return Err(LogQueryError::StoreMalformed);
                            }
                            let directory = required_store_entry(open_dir(&cases, &case_id))?;
                            scan_leaf(
                                &directory,
                                run,
                                LeafSelector::new(
                                    Some(check_name.clone()),
                                    phase,
                                    Some(case_id.clone()),
                                )
                                .map_err(|_| LogQueryError::StoreMalformed)?,
                                vec![
                                    run.run_id.clone(),
                                    "checks".into(),
                                    check_name.clone(),
                                    child.clone(),
                                    case_id,
                                ],
                                active,
                                allow_unsealed,
                                verify_streams,
                                &mut leaves,
                            )?;
                        }
                    }
                    _ => return Err(LogQueryError::StoreMalformed),
                }
            }
        }
    }
    Ok(leaves)
}

fn scan_executor_streams(
    run_dir: &File,
) -> Result<BTreeMap<LogStream, UnindexedStream>, LogQueryError> {
    let run_names = directory_names(run_dir)?;
    if !run_names.iter().any(|name| name == "executor") {
        return Ok(BTreeMap::new());
    }
    let directory = required_store_entry(open_dir(run_dir, "executor"))?;
    let names = directory_names(&directory)?;
    if names
        .iter()
        .any(|name| !matches!(name.as_str(), "stdout.log" | "stderr.log"))
    {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut streams = BTreeMap::new();
    for stream in [LogStream::Stdout, LogStream::Stderr] {
        let name = format!("{}.log", stream_name(stream));
        if !names.iter().any(|candidate| candidate == &name) {
            continue;
        }
        let file = required_store_entry(open_file(&directory, &name))?;
        let metadata = file.metadata().map_err(|_| LogQueryError::Unavailable)?;
        if !metadata.is_file() {
            return Err(LogQueryError::StoreMalformed);
        }
        let (lines, sha256) = inspect_complete_file(&file, metadata.len())?;
        streams.insert(
            stream,
            UnindexedStream {
                bytes: metadata.len(),
                lines,
                device: metadata.dev(),
                inode: metadata.ino(),
                sha256,
            },
        );
    }
    Ok(streams)
}

fn inspect_complete_file(file: &File, snapshot_bytes: u64) -> Result<(u64, String), LogQueryError> {
    let mut reader = file.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut remaining = snapshot_bytes;
    let mut last = None;
    let mut newlines = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    while remaining > 0 {
        let read = reader
            .read(&mut buffer[..remaining.min(READ_BLOCK_BYTES as u64) as usize])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        let block = &buffer[..read];
        hasher.update(block);
        newlines =
            newlines.saturating_add(block.iter().filter(|byte| **byte == b'\n').count() as u64);
        last = block.last().copied();
        remaining -= read as u64;
    }
    let lines = if snapshot_bytes == 0 {
        0
    } else {
        newlines + u64::from(last != Some(b'\n'))
    };
    Ok((lines, lower_hex(&hasher.finalize())))
}

#[allow(clippy::too_many_arguments)]
fn scan_leaf(
    leaf_dir: &File,
    run: &RunLogMetadata,
    selector: LeafSelector,
    relative_components: Vec<String>,
    active: bool,
    allow_unsealed: bool,
    verify_streams: bool,
    leaves: &mut Vec<InventoryLeaf>,
) -> Result<(), LogQueryError> {
    let names = directory_names(leaf_dir)?;
    let allowed = [
        "leaf.json",
        "stdout.log",
        "stdout.lines",
        "stdout.meta.json",
        "stderr.log",
        "stderr.lines",
        "stderr.meta.json",
        "diagnostics.json",
        "diagnostics",
        "evidence",
    ];
    if names.iter().any(|name| !allowed.contains(&name.as_str())) {
        return Err(LogQueryError::StoreMalformed);
    }
    let metadata: LeafLogMetadata = match read_json(leaf_dir, "leaf.json", MAX_METADATA_BYTES) {
        Ok(metadata) => metadata,
        Err(LogQueryError::LogNotFound) if allow_unsealed => LeafLogMetadata {
            schema: 2,
            selector: selector.clone(),
            status: devcoordinator2_executor_protocol::LeafStatus::Running,
            exit: devcoordinator2_executor_protocol::DiagnosticExit::default(),
            started_at_epoch_ms: run.started_at_epoch_ms,
            finished_at_epoch_ms: None,
            process_started: true,
            complete: false,
            structured_evidence_formats: Vec::new(),
            structured_evidence_count: 0,
        },
        Err(error) => {
            return Err(if error == LogQueryError::LogNotFound {
                LogQueryError::StoreMalformed
            } else {
                error
            });
        }
    };
    metadata
        .validate()
        .map_err(|_| LogQueryError::StoreMalformed)?;
    if metadata.selector != selector {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut streams = BTreeMap::new();
    let has_any_stream_entry = names.iter().any(|name| {
        matches!(
            name.as_str(),
            "stdout.log"
                | "stdout.lines"
                | "stdout.meta.json"
                | "stderr.log"
                | "stderr.lines"
                | "stderr.meta.json"
        )
    });
    if !verify_streams || (!active && !metadata.process_started && !has_any_stream_entry) {
        leaves.push(InventoryLeaf {
            run_id: run.run_id.clone(),
            test: run.test.clone(),
            selector,
            metadata,
            relative_components,
            directory_identity: file_identity(leaf_dir)?,
            active,
            streams,
        });
        return Ok(());
    }
    for stream in [LogStream::Stdout, LogStream::Stderr] {
        let stem = stream_name(stream);
        let metadata_name = format!("{stem}.meta.json");
        if !names.iter().any(|name| name == &metadata_name) {
            let log_name = format!("{stem}.log");
            if allow_unsealed && names.iter().any(|name| name == &log_name) {
                let file = required_store_entry(open_file(leaf_dir, &log_name))?;
                let details = file.metadata().map_err(|_| LogQueryError::Unavailable)?;
                if !details.is_file() || details.mode() & 0o077 != 0 {
                    return Err(LogQueryError::StoreMalformed);
                }
                let (lines, sha256) = inspect_complete_file(&file, details.len())?;
                streams.insert(
                    stream,
                    StreamMetadata {
                        schema: 2,
                        selector: selector.clone(),
                        stream,
                        bytes: details.len(),
                        lines,
                        first_write_epoch_ms: None,
                        last_write_epoch_ms: None,
                        complete: false,
                        truncated: false,
                        sha256,
                        line_index_stride: crate::log_store::LINE_INDEX_STRIDE,
                        line_index_format: "computed-active-v1".into(),
                    },
                );
                continue;
            }
            if allow_unsealed {
                continue;
            }
            return Err(LogQueryError::StoreMalformed);
        }
        let stream_metadata: StreamMetadata =
            required_store_entry(read_json(leaf_dir, &metadata_name, MAX_METADATA_BYTES))?;
        validate_stream_metadata(leaf_dir, &selector, stream, &stream_metadata)?;
        streams.insert(stream, stream_metadata);
    }
    leaves.push(InventoryLeaf {
        run_id: run.run_id.clone(),
        test: run.test.clone(),
        selector,
        metadata,
        relative_components,
        directory_identity: file_identity(leaf_dir)?,
        active,
        streams,
    });
    Ok(())
}

fn validate_stream_metadata(
    leaf_dir: &File,
    selector: &LeafSelector,
    stream: LogStream,
    metadata: &StreamMetadata,
) -> Result<(), LogQueryError> {
    if metadata.schema != 2
        || &metadata.selector != selector
        || metadata.stream != stream
        || metadata.truncated
        || metadata.line_index_stride != crate::log_store::LINE_INDEX_STRIDE
        || metadata.line_index_format != "u64le-line-byte-v1"
        || !valid_sha256(&metadata.sha256)
    {
        return Err(LogQueryError::StoreMalformed);
    }
    let stream_file =
        required_store_entry(open_file(leaf_dir, &format!("{}.log", stream_name(stream))))?;
    let stream_stat = stream_file
        .metadata()
        .map_err(|_| LogQueryError::Unavailable)?;
    if !stream_stat.is_file()
        || stream_stat.len() != metadata.bytes
        || stream_stat.mode() & 0o077 != 0
        || (metadata.bytes == 0
            && (metadata.lines != 0
                || metadata.first_write_epoch_ms.is_some()
                || metadata.last_write_epoch_ms.is_some()))
        || (metadata.bytes > 0
            && (metadata.lines == 0
                || metadata.lines > metadata.bytes
                || metadata.first_write_epoch_ms.is_none()
                || metadata.last_write_epoch_ms.is_none()))
        || metadata
            .first_write_epoch_ms
            .zip(metadata.last_write_epoch_ms)
            .is_some_and(|(first, last)| first > last || last > epoch_ms())
    {
        return Err(LogQueryError::StoreMalformed);
    }
    let index = required_store_entry(open_file(
        leaf_dir,
        &format!("{}.lines", stream_name(stream)),
    ))?;
    let index_stat = index.metadata().map_err(|_| LogQueryError::Unavailable)?;
    if !index_stat.is_file() || index_stat.mode() & 0o077 != 0 || index_stat.len() % 16 != 0 {
        return Err(LogQueryError::StoreMalformed);
    }
    let (actual_lines, actual_sha256) = inspect_complete_file(&stream_file, metadata.bytes)?;
    if actual_lines != metadata.lines || actual_sha256 != metadata.sha256 {
        return Err(LogQueryError::StoreMalformed);
    }
    validate_sparse_index(&stream_file, &index, metadata.bytes, metadata.lines)?;
    Ok(())
}

fn validate_sparse_index(
    stream: &File,
    index: &File,
    snapshot_bytes: u64,
    total_lines: u64,
) -> Result<(), LogQueryError> {
    let expected_records = if total_lines == 0 {
        0
    } else {
        (total_lines - 1) / crate::log_store::LINE_INDEX_STRIDE + 1
    };
    let index_metadata = index.metadata().map_err(|_| LogQueryError::Unavailable)?;
    if index_metadata.len() != expected_records.saturating_mul(16) {
        return Err(LogQueryError::StoreMalformed);
    }
    if snapshot_bytes == 0 {
        return Ok(());
    }
    let mut stream = stream.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    let mut index = index.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    stream
        .seek(SeekFrom::Start(0))
        .map_err(|_| LogQueryError::Unavailable)?;
    index
        .seek(SeekFrom::Start(0))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut position = 0_u64;
    let mut line = 1_u64;
    let mut at_line_start = true;
    let mut remaining = snapshot_bytes;
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    while remaining > 0 {
        let read = stream
            .read(&mut buffer[..remaining.min(READ_BLOCK_BYTES as u64) as usize])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        for byte in &buffer[..read] {
            if at_line_start {
                if (line - 1).is_multiple_of(crate::log_store::LINE_INDEX_STRIDE) {
                    let mut record = [0_u8; 16];
                    index
                        .read_exact(&mut record)
                        .map_err(|_| LogQueryError::StoreMalformed)?;
                    let indexed_line =
                        u64::from_le_bytes(record[..8].try_into().expect("line index width"));
                    let indexed_offset =
                        u64::from_le_bytes(record[8..].try_into().expect("line index width"));
                    if indexed_line != line || indexed_offset != position {
                        return Err(LogQueryError::StoreMalformed);
                    }
                }
                at_line_start = false;
            }
            position = position.saturating_add(1);
            if *byte == b'\n' {
                line = line.saturating_add(1);
                at_line_start = true;
            }
        }
        remaining -= read as u64;
    }
    Ok(())
}

fn query_catalog(
    inventory: &StoreInventory,
    run: &InventoryRun,
    request: &LogQueryRequest,
) -> Result<CatalogResult, LogQueryError> {
    let max_age_seconds = request
        .options
        .max_age_seconds
        .unwrap_or(crate::retention::DEFAULT_MAX_AGE_SECONDS);
    let history_depth = request
        .options
        .case_depth
        .unwrap_or(crate::retention::DEFAULT_HISTORY_DEPTH as u64);
    let policy = RetentionPolicy {
        max_age_seconds,
        history_depth: usize::try_from(history_depth).map_err(|_| LogQueryError::ArgsInvalid)?,
    };
    policy.validate().map_err(|_| LogQueryError::ArgsInvalid)?;

    let depth_ranks = depth_ranks(&inventory.runs);
    let mut entries = Vec::new();
    let executor_selector = LeafSelector::executor();
    if selector_matches(&request.selector, &executor_selector) {
        let executor_rank = executor_depth_rank(
            &inventory.runs,
            &run.metadata.run_id,
            &run.metadata.test,
            run.metadata.complete,
        );
        let retention_at = run_retention_epoch_ms(run);
        for (stream, snapshot) in &run.executor_streams {
            if request
                .selector
                .stream
                .is_some_and(|expected| expected != *stream)
            {
                continue;
            }
            entries.push(LogCatalogEntry {
                log_ref: log_ref(&run.metadata.run_id, &executor_selector, *stream),
                bytes: snapshot.bytes,
                lines: Some(snapshot.lines),
                first_byte_at: None,
                last_byte_at: None,
                complete: run.metadata.complete && !run.active,
                truncated: false,
                sha256: (run.metadata.complete && !run.active).then(|| snapshot.sha256.clone()),
                expires_at: retention_at.map(|value| {
                    iso_from_epoch_ms(
                        value.saturating_add(policy.max_age_seconds.saturating_mul(1_000)),
                    )
                }),
                depth_rank: executor_rank,
                structured_evidence: StructuredEvidenceSummary {
                    available: false,
                    formats: Vec::new(),
                    count: 0,
                },
            });
        }
    }
    for leaf in &run.leaves {
        if !selector_matches(&request.selector, &leaf.selector) {
            continue;
        }
        for (stream, metadata) in &leaf.streams {
            if request
                .selector
                .stream
                .is_some_and(|expected| expected != *stream)
            {
                continue;
            }
            let retention_at = leaf_retention_epoch_ms(run, leaf);
            let rank = retention_at.and_then(|_| {
                depth_ranks
                    .get(&(
                        leaf.run_id.clone(),
                        retention_history_key(&leaf.test, &leaf.selector, leaf.metadata.complete),
                    ))
                    .copied()
            });
            entries.push(LogCatalogEntry {
                log_ref: log_ref(&leaf.run_id, &leaf.selector, *stream),
                bytes: metadata.bytes,
                lines: Some(metadata.lines),
                first_byte_at: metadata.first_write_epoch_ms.map(iso_from_epoch_ms),
                last_byte_at: metadata.last_write_epoch_ms.map(iso_from_epoch_ms),
                complete: metadata.complete,
                truncated: false,
                sha256: metadata.complete.then(|| metadata.sha256.clone()),
                expires_at: retention_at.map(|value| {
                    iso_from_epoch_ms(
                        value.saturating_add(policy.max_age_seconds.saturating_mul(1_000)),
                    )
                }),
                depth_rank: rank,
                structured_evidence: StructuredEvidenceSummary {
                    available: leaf.metadata.structured_evidence_count > 0
                        || !leaf.metadata.structured_evidence_formats.is_empty(),
                    formats: leaf.metadata.structured_evidence_formats.clone(),
                    count: leaf.metadata.structured_evidence_count,
                },
            });
        }
    }
    entries.sort_by(|left, right| left.log_ref.cmp(&right.log_ref));
    let run_identity = file_identity(run.directory.as_ref().ok_or(LogQueryError::Unavailable)?)?;
    let digest = query_digest(request, &run.metadata.run_id)?;
    let mut start = 0usize;
    if let Some(encoded) = request.options.cursor.as_deref() {
        let cursor = decode_cursor(encoded)?;
        verify_catalog_cursor(
            &cursor,
            request.operation,
            &run.metadata.run_id,
            run_identity,
            entries.len() as u64,
            &digest,
        )?;
        start = usize::try_from(cursor.position).map_err(|_| LogQueryError::CursorStale)?;
        if start > entries.len() {
            return Err(LogQueryError::CursorStale);
        }
    }
    let limit = usize::try_from(request.options.limit.unwrap_or(100))
        .map_err(|_| LogQueryError::ArgsInvalid)?;
    let mut page = Vec::new();
    let mut index = start;
    while index < entries.len() && page.len() < limit {
        page.push(entries[index].clone());
        if serde_json::to_vec(&page)
            .map_err(|_| LogQueryError::Unavailable)?
            .len()
            > MAX_QUERY_CONTENT_BYTES
        {
            page.pop();
            break;
        }
        index += 1;
    }
    if index == start && start < entries.len() {
        return Err(LogQueryError::Unavailable);
    }
    let next_cursor = if index < entries.len() {
        Some(encode_cursor(&CursorPayload {
            schema: 2,
            operation: request.operation,
            log_ref: None,
            run_id: run.metadata.run_id.clone(),
            device: run_identity.0,
            inode: run_identity.1,
            snapshot_bytes: entries.len() as u64,
            snapshot_lines: 0,
            position: index as u64,
            byte_position: 0,
            auxiliary_position: 0,
            auxiliary_byte_position: 0,
            pending_line_end: None,
            emitted_through_line: 0,
            query_sha256: digest,
        })?)
    } else {
        None
    };
    Ok(CatalogResult {
        entries: page,
        next_cursor,
    })
}

fn query_tail(
    run: &InventoryRun,
    request: &LogQueryRequest,
) -> Result<ContentResult, LogQueryError> {
    let mut selected = select_stream(run, request, request.options.cursor.is_some())?;
    let max_bytes = validate_max_bytes(request.options.max_bytes.unwrap_or(32_768))?;
    let requested_lines = request.options.lines.unwrap_or(50);
    let digest = query_digest(request, &run.metadata.run_id)?;
    let cursor = content_cursor(request, &selected, &digest)?;
    clamp_to_cursor_snapshot(&mut selected, cursor.as_ref())?;
    if cursor.as_ref().is_some_and(|cursor| {
        cursor.auxiliary_byte_position != 0 || cursor.emitted_through_line != 0
    }) {
        return Err(LogQueryError::CursorStale);
    }
    let end_line = cursor
        .as_ref()
        .map_or(selected.metadata.lines, |cursor| cursor.position);
    if end_line == 0 || selected.metadata.lines == 0 {
        return Ok(ContentResult {
            segments: Vec::new(),
            snapshot_bytes: selected.snapshot_bytes,
            snapshot_lines: selected.metadata.lines,
            next_cursor: None,
            response_truncated: false,
        });
    }
    let end_line = end_line.min(selected.metadata.lines);
    let pending = cursor
        .as_ref()
        .filter(|cursor| cursor.pending_line_end.is_some());
    let window_start = pending
        .map(|cursor| cursor.auxiliary_position)
        .unwrap_or_else(|| end_line.saturating_sub(requested_lines - 1).max(1));
    if window_start == 0 || window_start > end_line {
        return Err(LogQueryError::CursorStale);
    }
    let interval_start = line_offset(&selected, window_start)?;
    let natural_interval_end = if end_line == selected.metadata.lines {
        selected.snapshot_bytes
    } else {
        line_offset(&selected, end_line.saturating_add(1))?
    };
    let interval_end = pending.map_or(natural_interval_end, |cursor| cursor.byte_position);
    if interval_end <= interval_start || interval_end > natural_interval_end {
        return Err(LogQueryError::CursorStale);
    }
    let tail_bytes = max_bytes.min(MAX_TEXT_BYTES);
    let byte_start = interval_end
        .saturating_sub(tail_bytes as u64)
        .max(interval_start);
    let bytes = read_bytes(
        &selected.file,
        byte_start,
        interval_end.saturating_sub(byte_start),
    )?;
    let (line_start, _) = line_for_byte(&selected, byte_start)?;
    let (line_end, _) = line_for_byte(&selected, interval_end.saturating_sub(1))?;
    let segment = LogSegment {
        line_start,
        line_end,
        byte_start,
        byte_end: interval_end,
        text: Some(String::from_utf8_lossy(&bytes).into_owned()),
        base64: None,
        rank: None,
        fingerprint: None,
        occurrences: None,
    };
    let omitted_in_window = byte_start > interval_start;
    let response_truncated = omitted_in_window || window_start > 1;
    let next_cursor = if omitted_in_window {
        Some(make_content_cursor_with_state(
            request,
            &selected,
            &digest,
            end_line,
            byte_start,
            window_start,
            0,
            Some(end_line),
            0,
        )?)
    } else if window_start > 1 {
        Some(make_content_cursor(
            request,
            &selected,
            &digest,
            window_start - 1,
            0,
        )?)
    } else {
        None
    };
    Ok(ContentResult {
        segments: vec![segment],
        snapshot_bytes: selected.snapshot_bytes,
        snapshot_lines: selected.metadata.lines,
        next_cursor,
        response_truncated,
    })
}

fn query_range(
    run: &InventoryRun,
    request: &LogQueryRequest,
) -> Result<ContentResult, LogQueryError> {
    let mut selected = select_stream(run, request, request.options.cursor.is_some())?;
    let max_bytes = validate_max_bytes(
        request
            .options
            .max_bytes
            .unwrap_or(MAX_QUERY_CONTENT_BYTES as u64),
    )?;
    let digest = query_digest(request, &run.metadata.run_id)?;
    let cursor = content_cursor(request, &selected, &digest)?;
    clamp_to_cursor_snapshot(&mut selected, cursor.as_ref())?;
    if cursor.as_ref().is_some_and(|cursor| {
        cursor.auxiliary_position != 0
            || cursor.auxiliary_byte_position != 0
            || cursor.pending_line_end.is_some()
            || cursor.emitted_through_line != 0
    }) {
        return Err(LogQueryError::CursorStale);
    }
    if let (Some(configured_start), Some(configured_end)) =
        (request.options.line_start, request.options.line_end)
    {
        let start = cursor
            .as_ref()
            .map_or(configured_start, |value| value.position);
        if start > selected.metadata.lines {
            return Ok(ContentResult {
                segments: Vec::new(),
                snapshot_bytes: selected.snapshot_bytes,
                snapshot_lines: selected.metadata.lines,
                next_cursor: None,
                response_truncated: false,
            });
        }
        let end = configured_end.min(selected.metadata.lines);
        let read = read_line_interval_from(
            &selected,
            start,
            end,
            max_bytes,
            cursor
                .as_ref()
                .filter(|value| value.byte_position > 0)
                .map(|value| value.byte_position),
        )?;
        let ended_mid_line = read.truncated && read.bytes.last().is_some_and(|byte| *byte != b'\n');
        let next_line = if ended_mid_line {
            read.line_end
        } else {
            read.line_end.saturating_add(1)
        };
        let next_byte = if ended_mid_line { read.byte_end } else { 0 };
        let next_cursor = if read.truncated && next_line <= end {
            Some(make_content_cursor(
                request, &selected, &digest, next_line, next_byte,
            )?)
        } else {
            None
        };
        let response_truncated = read.truncated;
        return Ok(ContentResult {
            segments: vec![read.segment(false)],
            snapshot_bytes: selected.snapshot_bytes,
            snapshot_lines: selected.metadata.lines,
            next_cursor,
            response_truncated,
        });
    }
    let configured_start = request.options.byte_start.unwrap_or(0);
    let configured_end = request.options.byte_end.unwrap_or(configured_start);
    let start = cursor
        .as_ref()
        .map_or(configured_start, |value| value.byte_position);
    let end = configured_end.min(selected.snapshot_bytes);
    if start > end {
        return Err(LogQueryError::CursorStale);
    }
    let take = end
        .saturating_sub(start)
        .min(max_bytes as u64)
        .min(MAX_BASE64_SOURCE_BYTES as u64);
    let bytes = read_bytes(&selected.file, start, take)?;
    let actual_end = start.saturating_add(bytes.len() as u64);
    let (line_start, _) = line_for_byte(&selected, start)?;
    let (line_end, _) = line_for_byte(&selected, actual_end.saturating_sub(1))?;
    let next_cursor = if actual_end < end {
        Some(make_content_cursor(
            request, &selected, &digest, line_end, actual_end,
        )?)
    } else {
        None
    };
    let segment = LogSegment {
        line_start,
        line_end,
        byte_start: start,
        byte_end: actual_end,
        text: None,
        base64: Some(base64_standard_encode(&bytes)),
        rank: None,
        fingerprint: None,
        occurrences: None,
    };
    Ok(ContentResult {
        segments: vec![segment],
        snapshot_bytes: selected.snapshot_bytes,
        snapshot_lines: selected.metadata.lines,
        next_cursor,
        response_truncated: actual_end < end,
    })
}

fn query_search(
    run: &InventoryRun,
    request: &LogQueryRequest,
) -> Result<SearchResult, LogQueryError> {
    let mut selected = select_stream(run, request, request.options.cursor.is_some())?;
    let max_bytes = validate_max_bytes(request.options.max_bytes.unwrap_or(32_768))?;
    let text = request
        .options
        .text
        .as_deref()
        .ok_or(LogQueryError::ArgsInvalid)?;
    let needle = text.as_bytes();
    let maximum = usize::try_from(request.options.max_matches.unwrap_or(20))
        .map_err(|_| LogQueryError::ArgsInvalid)?;
    let context = request.options.context_lines.unwrap_or(2);
    let digest = query_digest(request, &run.metadata.run_id)?;
    let cursor = content_cursor(request, &selected, &digest)?;
    clamp_to_cursor_snapshot(&mut selected, cursor.as_ref())?;
    let mut matches = Vec::new();
    let mut used = 0usize;
    let mut emitted_through = cursor
        .as_ref()
        .map_or(0, |value| value.emitted_through_line);
    let mut scan_line = cursor.as_ref().map_or(1, |value| value.position.max(1));
    let mut scan_byte = cursor.as_ref().map_or(0, |value| value.byte_position);
    let mut next_state: Option<(u64, u64, u64, u64, Option<u64>, u64)> = None;

    if let Some(cursor) = cursor.as_ref()
        && let Some(pending_end) = cursor.pending_line_end
    {
        if cursor.auxiliary_position == 0 || cursor.position == 0 || pending_end < cursor.position {
            return Err(LogQueryError::CursorStale);
        }
        let read = read_line_interval_from(
            &selected,
            cursor.position,
            pending_end,
            (max_bytes - used).min(4 * 1024),
            Some(cursor.byte_position),
        )?;
        used = used.saturating_add(read.bytes.len());
        let read_truncated = read.truncated;
        let read_byte_end = read.byte_end;
        let ended_mid_line = read_truncated && read.bytes.last().is_some_and(|byte| *byte != b'\n');
        let next_line = if ended_mid_line {
            read.line_end
        } else {
            read.line_end.saturating_add(1)
        };
        let segment = read.segment(false);
        if !search_page_fits(
            std::slice::from_ref(&segment),
            selected.snapshot_bytes,
            selected.metadata.lines,
        )? {
            return Err(LogQueryError::Unavailable);
        }
        matches.push(segment);
        if read_truncated && next_line <= pending_end {
            next_state = Some((
                next_line,
                read_byte_end,
                cursor.auxiliary_position,
                cursor.auxiliary_byte_position,
                Some(pending_end),
                emitted_through,
            ));
        } else {
            emitted_through = emitted_through.max(pending_end);
            scan_line = cursor.auxiliary_position;
            scan_byte = cursor.auxiliary_byte_position;
        }
    } else if cursor.as_ref().is_some_and(|cursor| {
        cursor.auxiliary_position != 0
            || cursor.auxiliary_byte_position != 0
            || cursor.pending_line_end.is_some()
    }) {
        return Err(LogQueryError::CursorStale);
    }

    if next_state.is_none() && scan_byte < selected.snapshot_bytes {
        let scan = find_literal_lines(
            &selected.file,
            selected.snapshot_bytes,
            scan_byte,
            scan_line,
            needle,
            maximum.saturating_add(1),
        )?;
        let returned_len = scan.len().min(maximum);
        for index in 0..returned_len {
            let hit = scan[index];
            let resume = scan.get(index + 1).copied().unwrap_or(LineHit {
                line: selected.metadata.lines.saturating_add(1),
                byte_start: selected.snapshot_bytes,
            });
            let start = hit
                .line
                .saturating_sub(context)
                .max(1)
                .max(emitted_through.saturating_add(1));
            let end = hit
                .line
                .saturating_add(context)
                .min(selected.metadata.lines);
            if start > end {
                continue;
            }
            if used >= max_bytes {
                next_state = Some((hit.line, hit.byte_start, 0, 0, None, emitted_through));
                break;
            }
            let read = read_line_interval(&selected, start, end, (max_bytes - used).min(4 * 1024))?;
            let read_truncated = read.truncated;
            let read_len = read.bytes.len();
            let ended_mid_line =
                read_truncated && read.bytes.last().is_some_and(|byte| *byte != b'\n');
            let next_line = if ended_mid_line {
                read.line_end
            } else {
                read.line_end.saturating_add(1)
            };
            let read_byte_end = read.byte_end;
            let segment = read.segment(false);
            let mut trial = matches.clone();
            trial.push(segment.clone());
            if !search_page_fits(&trial, selected.snapshot_bytes, selected.metadata.lines)? {
                next_state = Some((hit.line, hit.byte_start, 0, 0, None, emitted_through));
                break;
            }
            used = used.saturating_add(read_len);
            matches.push(segment);
            if read_truncated && next_line <= end {
                next_state = Some((
                    next_line,
                    read_byte_end,
                    resume.line,
                    resume.byte_start,
                    Some(end),
                    emitted_through,
                ));
                break;
            }
            emitted_through = emitted_through.max(end);
        }
        if next_state.is_none() && scan.len() > maximum {
            let next = scan[maximum];
            next_state = Some((next.line, next.byte_start, 0, 0, None, emitted_through));
        }
    }
    let next_cursor = next_state
        .map(|(line, byte, aux_line, aux_byte, pending_end, emitted)| {
            make_content_cursor_with_state(
                request,
                &selected,
                &digest,
                line,
                byte,
                aux_line,
                aux_byte,
                pending_end,
                emitted,
            )
        })
        .transpose()?;
    Ok(SearchResult {
        matches,
        snapshot_bytes: selected.snapshot_bytes,
        snapshot_lines: selected.metadata.lines,
        response_truncated: next_cursor.is_some(),
        next_cursor,
    })
}

fn search_page_fits(
    matches: &[LogSegment],
    snapshot_bytes: u64,
    snapshot_lines: u64,
) -> Result<bool, LogQueryError> {
    let reserved_cursor = "x".repeat(MAX_CURSOR_BYTES);
    let result = SearchResult {
        matches: matches.to_vec(),
        snapshot_bytes,
        snapshot_lines,
        next_cursor: Some(reserved_cursor),
        response_truncated: true,
    };
    Ok(serde_json::to_vec(&result)
        .map_err(|_| LogQueryError::Unavailable)?
        .len()
        <= MAX_QUERY_RESULT_BYTES)
}

fn query_failure_context(
    run: &InventoryRun,
    request: &LogQueryRequest,
) -> Result<FailureContextResult, LogQueryError> {
    let limit = usize::try_from(request.options.limit.unwrap_or(20))
        .map_err(|_| LogQueryError::ArgsInvalid)?;
    let context_lines = request.options.context_lines.unwrap_or(2);
    let max_bytes = validate_max_bytes(request.options.max_bytes.unwrap_or(32_768))?;
    let digest = query_digest(request, &run.metadata.run_id)?;
    let mut failures = load_structured_failures(run, &request.selector)?;
    failures.sort_by(|left, right| {
        diagnostic_rank(left)
            .cmp(&diagnostic_rank(right))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    let mut selected = select_stream(run, request, request.options.cursor.is_some())?;
    let cursor = content_cursor(request, &selected, &digest)?;
    clamp_to_cursor_snapshot(&mut selected, cursor.as_ref())?;
    if cursor.as_ref().is_some_and(|cursor| {
        cursor.auxiliary_byte_position != 0 || cursor.emitted_through_line != 0
    }) {
        return Err(LogQueryError::CursorStale);
    }
    let hits = find_failure_lines(
        &selected.file,
        selected.snapshot_bytes,
        0,
        1,
        MAX_DIAGNOSTIC_EVENTS,
    )?;
    let intervals = merge_ranked_intervals(&hits, context_lines, selected.metadata.lines);
    let total_items = failures.len().saturating_add(intervals.len());
    let mut item = usize::try_from(cursor.as_ref().map_or(0, |cursor| cursor.position))
        .map_err(|_| LogQueryError::CursorStale)?;
    if item > total_items {
        return Err(LogQueryError::CursorStale);
    }
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.pending_line_end.is_some() && item < failures.len())
    {
        return Err(LogQueryError::CursorStale);
    }
    let mut returned_failures = Vec::new();
    let mut contexts = Vec::new();
    let mut used_bytes = 0usize;
    let mut returned_items = 0usize;
    let mut next_partial: Option<(u64, u64)> = None;
    while item < total_items && returned_items < limit {
        if item < failures.len() {
            let candidate = failures[item].clone();
            let mut trial = returned_failures.clone();
            trial.push(candidate.clone());
            if !failure_page_fits(
                &trial,
                &contexts,
                selected.snapshot_bytes,
                selected.metadata.lines,
            )? {
                if returned_items == 0 {
                    return Err(LogQueryError::Unavailable);
                }
                break;
            }
            returned_failures.push(candidate);
            item += 1;
            returned_items += 1;
            continue;
        }
        let interval = &intervals[item - failures.len()];
        if used_bytes >= max_bytes {
            break;
        }
        let (start_line, byte_override) = if let Some(cursor) = cursor.as_ref()
            && item == usize::try_from(cursor.position).unwrap_or(usize::MAX)
            && let Some(pending_end) = cursor.pending_line_end
        {
            if pending_end != interval.end
                || cursor.auxiliary_position < interval.start
                || cursor.auxiliary_position > interval.end
            {
                return Err(LogQueryError::CursorStale);
            }
            (cursor.auxiliary_position, Some(cursor.byte_position))
        } else {
            (interval.start, None)
        };
        let read = read_line_interval_from(
            &selected,
            start_line,
            interval.end,
            (max_bytes - used_bytes).min(4 * 1024),
            byte_override,
        )?;
        let read_truncated = read.truncated;
        let read_len = read.bytes.len();
        let read_byte_end = read.byte_end;
        let ended_mid_line = read_truncated && read.bytes.last().is_some_and(|byte| *byte != b'\n');
        let next_line = if ended_mid_line {
            read.line_end
        } else {
            read.line_end.saturating_add(1)
        };
        let mut segment = read.segment(false);
        segment.rank = Some(interval.rank);
        segment.fingerprint = Some(interval.fingerprint.clone());
        segment.occurrences = Some(interval.occurrences);
        let mut trial = contexts.clone();
        trial.push(segment.clone());
        if !failure_page_fits(
            &returned_failures,
            &trial,
            selected.snapshot_bytes,
            selected.metadata.lines,
        )? {
            if returned_items == 0 {
                return Err(LogQueryError::Unavailable);
            }
            break;
        }
        contexts.push(segment);
        used_bytes = used_bytes.saturating_add(read_len);
        returned_items += 1;
        if read_truncated && next_line <= interval.end {
            next_partial = Some((next_line, read_byte_end));
            break;
        }
        item += 1;
    }
    let has_more = next_partial.is_some() || item < total_items;
    let next_cursor = if has_more {
        let (pending_line, pending_byte, pending_end) = next_partial
            .map(|(line, byte)| (line, byte, Some(intervals[item - failures.len()].end)))
            .unwrap_or((0, 0, None));
        Some(make_content_cursor_with_state(
            request,
            &selected,
            &digest,
            u64::try_from(item).map_err(|_| LogQueryError::Unavailable)?,
            pending_byte,
            pending_line,
            0,
            pending_end,
            0,
        )?)
    } else {
        None
    };
    Ok(FailureContextResult {
        failures: returned_failures,
        contexts,
        snapshot_bytes: Some(selected.snapshot_bytes),
        snapshot_lines: Some(selected.metadata.lines),
        next_cursor,
        response_truncated: has_more,
    })
}

fn failure_page_fits(
    failures: &[FailureIndexEntry],
    contexts: &[LogSegment],
    snapshot_bytes: u64,
    snapshot_lines: u64,
) -> Result<bool, LogQueryError> {
    let result = FailureContextResult {
        failures: failures.to_vec(),
        contexts: contexts.to_vec(),
        snapshot_bytes: Some(snapshot_bytes),
        snapshot_lines: Some(snapshot_lines),
        next_cursor: Some("x".repeat(MAX_CURSOR_BYTES)),
        response_truncated: true,
    };
    Ok(serde_json::to_vec(&result)
        .map_err(|_| LogQueryError::Unavailable)?
        .len()
        <= MAX_QUERY_RESULT_BYTES)
}

struct SelectedStream {
    log_ref: LogRef,
    metadata: StreamMetadata,
    file: File,
    index: Option<File>,
    device: u64,
    inode: u64,
    snapshot_bytes: u64,
}

fn select_stream(
    run: &InventoryRun,
    request: &LogQueryRequest,
    expired_when_missing: bool,
) -> Result<SelectedStream, LogQueryError> {
    let phase = request.selector.phase.ok_or(LogQueryError::ArgsInvalid)?;
    let stream = request.selector.stream.ok_or(LogQueryError::ArgsInvalid)?;
    let selector = LeafSelector::new(
        request.selector.check.clone(),
        phase,
        request.selector.case_id.clone(),
    )
    .map_err(|_| LogQueryError::ArgsInvalid)?;
    if selector.phase == LogPhase::Executor {
        let snapshot = run
            .executor_streams
            .get(&stream)
            .ok_or(LogQueryError::LogNotFound)?;
        let directory = open_dir(
            run.directory.as_ref().ok_or(LogQueryError::Unavailable)?,
            "executor",
        )?;
        let file = open_file(&directory, &format!("{}.log", stream_name(stream)))?;
        return Ok(SelectedStream {
            log_ref: log_ref(&run.metadata.run_id, &selector, stream),
            metadata: StreamMetadata {
                schema: 2,
                selector,
                stream,
                bytes: snapshot.bytes,
                lines: snapshot.lines,
                first_write_epoch_ms: None,
                last_write_epoch_ms: None,
                complete: run.metadata.complete && !run.active,
                truncated: false,
                sha256: snapshot.sha256.clone(),
                line_index_stride: crate::log_store::LINE_INDEX_STRIDE,
                line_index_format: "computed-snapshot-v1".into(),
            },
            file,
            index: None,
            device: snapshot.device,
            inode: snapshot.inode,
            snapshot_bytes: snapshot.bytes,
        });
    }
    let Some(leaf) = run.leaves.iter().find(|leaf| leaf.selector == selector) else {
        return Err(if expired_when_missing {
            LogQueryError::LogExpired
        } else {
            LogQueryError::LogNotFound
        });
    };
    open_selected_stream(run, leaf, stream)
}

fn open_selected_stream(
    run: &InventoryRun,
    leaf: &InventoryLeaf,
    stream: LogStream,
) -> Result<SelectedStream, LogQueryError> {
    let metadata = leaf
        .streams
        .get(&stream)
        .cloned()
        .ok_or(LogQueryError::LogNotFound)?;
    let leaf_dir = open_leaf_components(
        run.directory.as_ref().ok_or(LogQueryError::Unavailable)?,
        &leaf.relative_components[1..],
    )?;
    let file = open_file(&leaf_dir, &format!("{}.log", stream_name(stream)))?;
    let index = if metadata.line_index_format == "computed-active-v1" {
        None
    } else {
        Some(open_file(
            &leaf_dir,
            &format!("{}.lines", stream_name(stream)),
        )?)
    };
    let identity = file_identity(&file)?;
    let size = file
        .metadata()
        .map_err(|_| LogQueryError::Unavailable)?
        .len();
    if size < metadata.bytes || (!leaf.active && size != metadata.bytes) {
        return Err(LogQueryError::StoreMalformed);
    }
    let snapshot_bytes = metadata.bytes.min(size);
    Ok(SelectedStream {
        log_ref: log_ref(&leaf.run_id, &leaf.selector, stream),
        metadata,
        file,
        index,
        device: identity.0,
        inode: identity.1,
        snapshot_bytes,
    })
}

fn load_structured_failures(
    run: &InventoryRun,
    filter: &LogQuerySelector,
) -> Result<Vec<FailureIndexEntry>, LogQueryError> {
    let mut grouped = BTreeMap::<String, FailureIndexEntry>::new();
    for leaf in &run.leaves {
        if !selector_matches(filter, &leaf.selector) || leaf.metadata.structured_evidence_count == 0
        {
            continue;
        }
        let leaf_dir = open_leaf_components(
            run.directory.as_ref().ok_or(LogQueryError::Unavailable)?,
            &leaf.relative_components[1..],
        )?;
        let stored: StoredDiagnostics =
            read_json(&leaf_dir, "diagnostics.json", MAX_DIAGNOSTICS_BYTES).map_err(|error| {
                if error == LogQueryError::LogNotFound {
                    LogQueryError::StoreMalformed
                } else {
                    error
                }
            })?;
        if stored.schema != 2
            || stored.check != leaf.selector.check
            || stored.case_id != leaf.selector.case_id
            || stored.entries.len() as u64 != leaf.metadata.structured_evidence_count
        {
            return Err(LogQueryError::StoreMalformed);
        }
        for mut entry in stored.entries {
            entry
                .validate()
                .map_err(|_| LogQueryError::StoreMalformed)?;
            let case_is_bound = leaf.selector.case_id.as_deref().is_none_or(|case_id| {
                entry.case.as_deref().is_some_and(|reported| {
                    reported == case_id
                        || reported
                            .strip_prefix(case_id)
                            .is_some_and(|suffix| suffix.starts_with(" :: "))
                })
            });
            if entry.check != leaf.selector.check
                || !case_is_bound
                || entry.fingerprint != diagnostic_fingerprint(&entry)
                || entry.log_refs.iter().any(|reference| {
                    reference.run_id != leaf.run_id
                        || reference.check != leaf.selector.check
                        || reference.phase != leaf.selector.phase
                        || reference.case != leaf.selector.case_id
                })
            {
                return Err(LogQueryError::StoreMalformed);
            }
            if let Some(existing) = grouped.get_mut(&entry.fingerprint) {
                existing.occurrences = existing.occurrences.saturating_add(entry.occurrences);
                for reference in entry.log_refs.drain(..) {
                    if !existing.log_refs.contains(&reference) {
                        existing.log_refs.push(reference);
                    }
                }
                existing.log_refs.sort();
            } else {
                grouped.insert(entry.fingerprint.clone(), entry);
            }
        }
    }
    Ok(grouped.into_values().collect())
}

#[derive(Clone, Copy)]
struct LineHit {
    line: u64,
    byte_start: u64,
}

#[derive(Clone)]
struct RankedHit {
    line: u64,
    rank: u8,
    fingerprint: String,
    occurrences: u64,
}

#[derive(Clone)]
struct RankedInterval {
    start: u64,
    end: u64,
    rank: u8,
    fingerprint: String,
    occurrences: u64,
}

struct LineRead {
    line_start: u64,
    line_end: u64,
    byte_start: u64,
    byte_end: u64,
    bytes: Vec<u8>,
    truncated: bool,
}

impl LineRead {
    fn segment(self, exact_binary: bool) -> LogSegment {
        let binary = exact_binary && std::str::from_utf8(&self.bytes).is_err();
        LogSegment {
            line_start: self.line_start,
            line_end: self.line_end,
            byte_start: self.byte_start,
            byte_end: self.byte_end,
            text: (!binary).then(|| String::from_utf8_lossy(&self.bytes).into_owned()),
            base64: binary.then(|| base64_standard_encode(&self.bytes)),
            rank: None,
            fingerprint: None,
            occurrences: None,
        }
    }
}

fn read_line_interval(
    selected: &SelectedStream,
    start_line: u64,
    end_line: u64,
    max_bytes: usize,
) -> Result<LineRead, LogQueryError> {
    read_line_interval_from(selected, start_line, end_line, max_bytes, None)
}

fn read_line_interval_from(
    selected: &SelectedStream,
    start_line: u64,
    end_line: u64,
    max_bytes: usize,
    byte_override: Option<u64>,
) -> Result<LineRead, LogQueryError> {
    let max_bytes = max_bytes.min(MAX_TEXT_BYTES);
    if start_line == 0 || end_line < start_line || max_bytes == 0 {
        return Err(LogQueryError::ArgsInvalid);
    }
    let natural_start = line_offset(selected, start_line)?;
    let byte_start = byte_override.unwrap_or(natural_start);
    if byte_start < natural_start || byte_start > selected.snapshot_bytes {
        return Err(LogQueryError::CursorStale);
    }
    let mut file = selected
        .file
        .try_clone()
        .map_err(|_| LogQueryError::Unavailable)?;
    file.seek(SeekFrom::Start(byte_start))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(READ_BLOCK_BYTES));
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    let mut position = byte_start;
    let mut line = start_line;
    let mut actual_end_line = start_line;
    let mut truncated = false;
    'outer: while position < selected.snapshot_bytes && line <= end_line {
        let remaining = selected.snapshot_bytes - position;
        let read = file
            .read(&mut buffer[..remaining.min(READ_BLOCK_BYTES as u64) as usize])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        for byte in &buffer[..read] {
            if bytes.len() == max_bytes {
                truncated = true;
                break 'outer;
            }
            bytes.push(*byte);
            position += 1;
            actual_end_line = line;
            if *byte == b'\n' {
                if line == end_line {
                    break 'outer;
                }
                line = line.saturating_add(1);
            }
        }
    }
    Ok(LineRead {
        line_start: start_line,
        line_end: actual_end_line.min(end_line),
        byte_start,
        byte_end: position,
        bytes,
        truncated,
    })
}

fn line_offset(selected: &SelectedStream, target_line: u64) -> Result<u64, LogQueryError> {
    if target_line == 0 || target_line > selected.metadata.lines {
        return Err(LogQueryError::ArgsInvalid);
    }
    let (mut line, mut offset) = if let Some(index_file) = &selected.index {
        let stride = selected.metadata.line_index_stride;
        let record_index = (target_line - 1) / stride;
        let mut index = index_file
            .try_clone()
            .map_err(|_| LogQueryError::Unavailable)?;
        index
            .seek(SeekFrom::Start(record_index.saturating_mul(16)))
            .map_err(|_| LogQueryError::Unavailable)?;
        let mut record = [0_u8; 16];
        index
            .read_exact(&mut record)
            .map_err(|_| LogQueryError::StoreMalformed)?;
        (
            u64::from_le_bytes(record[..8].try_into().expect("line index width")),
            u64::from_le_bytes(record[8..].try_into().expect("line index width")),
        )
    } else {
        (1, 0)
    };
    if line == 0 || line > target_line || offset > selected.snapshot_bytes {
        return Err(LogQueryError::StoreMalformed);
    }
    if line == target_line {
        return Ok(offset);
    }
    let mut file = selected
        .file
        .try_clone()
        .map_err(|_| LogQueryError::Unavailable)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    while offset < selected.snapshot_bytes {
        let remaining = selected.snapshot_bytes - offset;
        let read = file
            .read(&mut buffer[..remaining.min(READ_BLOCK_BYTES as u64) as usize])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            offset += 1;
            if *byte == b'\n' {
                line += 1;
                if line == target_line {
                    return Ok(offset);
                }
            }
        }
    }
    Err(LogQueryError::StoreMalformed)
}

fn line_for_byte(selected: &SelectedStream, target: u64) -> Result<(u64, u64), LogQueryError> {
    if selected.snapshot_bytes == 0 {
        return Ok((0, 0));
    }
    let target = target.min(selected.snapshot_bytes - 1);
    let mut chosen = (1_u64, 0_u64);
    if let Some(index_file) = &selected.index {
        let records = index_file
            .metadata()
            .map_err(|_| LogQueryError::Unavailable)?
            .len()
            / 16;
        if records == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        let mut low = 0_u64;
        let mut high = records;
        let mut index = index_file
            .try_clone()
            .map_err(|_| LogQueryError::Unavailable)?;
        while low < high {
            let middle = low + (high - low) / 2;
            index
                .seek(SeekFrom::Start(middle * 16))
                .map_err(|_| LogQueryError::Unavailable)?;
            let mut record = [0_u8; 16];
            index
                .read_exact(&mut record)
                .map_err(|_| LogQueryError::StoreMalformed)?;
            let line = u64::from_le_bytes(record[..8].try_into().expect("line index width"));
            let offset = u64::from_le_bytes(record[8..].try_into().expect("line index width"));
            if offset <= target {
                chosen = (line, offset);
                low = middle + 1;
            } else {
                high = middle;
            }
        }
    }
    let mut file = selected
        .file
        .try_clone()
        .map_err(|_| LogQueryError::Unavailable)?;
    file.seek(SeekFrom::Start(chosen.1))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut line = chosen.0;
    let mut position = chosen.1;
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    while position < target {
        let read = file
            .read(&mut buffer[..((target - position) as usize).min(READ_BLOCK_BYTES)])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        for byte in &buffer[..read] {
            position += 1;
            if *byte == b'\n' {
                line += 1;
            }
        }
    }
    Ok((line.min(selected.metadata.lines), chosen.1))
}

fn read_bytes(file: &File, start: u64, count: u64) -> Result<Vec<u8>, LogQueryError> {
    let count = usize::try_from(count).map_err(|_| LogQueryError::ArgsInvalid)?;
    let mut file = file.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    file.seek(SeekFrom::Start(start))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut output = vec![0_u8; count];
    file.read_exact(&mut output)
        .map_err(|_| LogQueryError::StoreMalformed)?;
    Ok(output)
}

fn find_literal_lines(
    file: &File,
    snapshot_bytes: u64,
    start_byte: u64,
    start_line: u64,
    needle: &[u8],
    maximum: usize,
) -> Result<Vec<LineHit>, LogQueryError> {
    if needle.is_empty() || start_byte > snapshot_bytes {
        return Err(LogQueryError::ArgsInvalid);
    }
    let prefix = kmp_prefix(needle);
    let mut matched = 0usize;
    let mut line_matched = false;
    let mut line = start_line;
    let mut line_start = start_byte;
    let mut position = start_byte;
    let mut output = Vec::new();
    let mut reader = file.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    reader
        .seek(SeekFrom::Start(start_byte))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut buffer = [0_u8; READ_BLOCK_BYTES];
    while position < snapshot_bytes && output.len() < maximum {
        let remaining = snapshot_bytes - position;
        let read = reader
            .read(&mut buffer[..remaining.min(READ_BLOCK_BYTES as u64) as usize])
            .map_err(|_| LogQueryError::Unavailable)?;
        if read == 0 {
            return Err(LogQueryError::StoreMalformed);
        }
        for byte in &buffer[..read] {
            if *byte != b'\n' {
                while matched > 0 && needle[matched] != *byte {
                    matched = prefix[matched - 1];
                }
                if needle[matched] == *byte {
                    matched += 1;
                    if matched == needle.len() {
                        line_matched = true;
                        matched = prefix[matched - 1];
                    }
                }
            }
            position += 1;
            if *byte == b'\n' {
                if line_matched {
                    output.push(LineHit {
                        line,
                        byte_start: line_start,
                    });
                    if output.len() == maximum {
                        break;
                    }
                }
                line += 1;
                line_start = position;
                matched = 0;
                line_matched = false;
            }
        }
    }
    if position == snapshot_bytes
        && line_start < snapshot_bytes
        && line_matched
        && output.len() < maximum
    {
        output.push(LineHit {
            line,
            byte_start: line_start,
        });
    }
    Ok(output)
}

fn kmp_prefix(needle: &[u8]) -> Vec<usize> {
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0usize;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
            prefix[index] = matched;
        }
    }
    prefix
}

fn find_failure_lines(
    file: &File,
    snapshot_bytes: u64,
    start_byte: u64,
    start_line: u64,
    maximum: usize,
) -> Result<Vec<RankedHit>, LogQueryError> {
    let mut reader = BufReader::new(file.try_clone().map_err(|_| LogQueryError::Unavailable)?);
    reader
        .seek(SeekFrom::Start(start_byte))
        .map_err(|_| LogQueryError::Unavailable)?;
    let mut remaining = snapshot_bytes.saturating_sub(start_byte);
    let mut line = start_line;
    let mut unique = BTreeMap::<String, RankedHit>::new();
    let mut worst = BinaryHeap::<(u8, u64, String)>::new();
    let mut recognized_lines = BTreeSet::new();
    let mut final_lines = VecDeque::<(u64, Vec<u8>)>::new();
    while remaining > 0 {
        let mut raw = Vec::new();
        let mut byte = [0_u8; 1];
        while remaining > 0 {
            let read = reader
                .read(&mut byte)
                .map_err(|_| LogQueryError::Unavailable)?;
            if read == 0 {
                return Err(LogQueryError::StoreMalformed);
            }
            remaining -= 1;
            if raw.len() < 4096 {
                raw.push(byte[0]);
            }
            if byte[0] == b'\n' {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw);
        if !text.trim().is_empty() {
            final_lines.push_back((line, raw.clone()));
            while final_lines.len() > 3 {
                final_lines.pop_front();
            }
        }
        if let Some(rank) = recognized_rank(&text) {
            recognized_lines.insert(line);
            let normalized = text.trim().to_ascii_lowercase();
            let fingerprint = format!(
                "sha256:{}",
                lower_hex(&Sha256::digest(
                    [rank]
                        .into_iter()
                        .chain(normalized.bytes())
                        .collect::<Vec<_>>(),
                ))
            );
            insert_ranked_hit(
                &mut unique,
                &mut worst,
                RankedHit {
                    line,
                    rank,
                    fingerprint,
                    occurrences: 1,
                },
                maximum,
            );
        }
        line += 1;
    }
    for (line, raw) in final_lines {
        if recognized_lines.contains(&line) {
            continue;
        }
        let normalized = String::from_utf8_lossy(&raw).trim().to_ascii_lowercase();
        let fingerprint = format!(
            "sha256:{}",
            lower_hex(&Sha256::digest(
                [7_u8]
                    .into_iter()
                    .chain(normalized.bytes())
                    .collect::<Vec<_>>(),
            ))
        );
        insert_ranked_hit(
            &mut unique,
            &mut worst,
            RankedHit {
                line,
                rank: 7,
                fingerprint,
                occurrences: 1,
            },
            maximum,
        );
    }
    let mut output: Vec<_> = unique.into_values().collect();
    output.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    output.truncate(maximum);
    Ok(output)
}

fn insert_ranked_hit(
    unique: &mut BTreeMap<String, RankedHit>,
    worst: &mut BinaryHeap<(u8, u64, String)>,
    candidate: RankedHit,
    maximum: usize,
) {
    if let Some(existing) = unique.get_mut(&candidate.fingerprint) {
        existing.occurrences = existing.occurrences.saturating_add(candidate.occurrences);
        return;
    }
    if maximum == 0 {
        return;
    }
    let key = (
        candidate.rank,
        candidate.line,
        candidate.fingerprint.clone(),
    );
    if unique.len() >= maximum {
        let Some(current_worst) = worst.peek() else {
            return;
        };
        if key >= *current_worst {
            return;
        }
        if let Some((_, _, fingerprint)) = worst.pop() {
            unique.remove(&fingerprint);
        }
    }
    worst.push(key);
    unique.insert(candidate.fingerprint.clone(), candidate);
}

fn recognized_rank(text: &str) -> Option<u8> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("assertionerror")
        || lower.contains("assertion failed")
        || lower.contains("assert_eq!")
        || lower.contains("expected:")
    {
        Some(1)
    } else if lower.starts_with("error[")
        || lower.contains(": error:")
        || lower.contains("compiler error")
    {
        Some(2)
    } else if lower.contains("panicked at")
        || lower.contains("traceback (most recent call last)")
        || lower.contains("exception:")
    {
        Some(3)
    } else if lower.trim_start().starts_with("at ") || lower.contains("stack backtrace:") {
        Some(4)
    } else if lower.contains("timed out")
        || lower.contains("deadline exceeded")
        || lower.contains("process exited")
        || lower.contains("terminated by signal")
    {
        Some(5)
    } else if lower.contains("console.error")
        || lower.contains("net::err_")
        || lower.contains("failed network request")
    {
        Some(6)
    } else {
        None
    }
}

fn merge_ranked_intervals(
    hits: &[RankedHit],
    context: u64,
    total_lines: u64,
) -> Vec<RankedInterval> {
    let mut candidates: Vec<_> = hits
        .iter()
        .map(|hit| RankedInterval {
            start: hit.line.saturating_sub(context).max(1),
            end: hit.line.saturating_add(context).min(total_lines),
            rank: hit.rank,
            fingerprint: hit.fingerprint.clone(),
            occurrences: hit.occurrences,
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| left.end.cmp(&right.end))
            .then_with(|| left.rank.cmp(&right.rank))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    let mut merged: Vec<RankedInterval> = Vec::new();
    for candidate in candidates {
        if let Some(previous) = merged.last_mut()
            && candidate.start <= previous.end
        {
            previous.end = previous.end.max(candidate.end);
            previous.rank = previous.rank.min(candidate.rank);
            previous.occurrences = previous.occurrences.saturating_add(candidate.occurrences);
            let mut fingerprints = [
                previous.fingerprint.as_str(),
                candidate.fingerprint.as_str(),
            ];
            fingerprints.sort_unstable();
            previous.fingerprint = format!(
                "sha256:{}",
                lower_hex(&Sha256::digest(fingerprints.join("\0").as_bytes(),)),
            );
        } else {
            merged.push(candidate);
        }
    }
    merged.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.start.cmp(&right.start))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    merged
}

fn selector_matches(filter: &LogQuerySelector, selector: &LeafSelector) -> bool {
    filter.check.as_ref().is_none_or(|value| {
        selector
            .check
            .as_ref()
            .is_some_and(|actual| actual == value)
    }) && filter.phase.is_none_or(|value| selector.phase == value)
        && filter.case_id.as_ref().is_none_or(|value| {
            selector
                .case_id
                .as_ref()
                .is_some_and(|actual| actual == value)
        })
}

fn history_key(test: &str, selector: &LeafSelector) -> String {
    format!(
        "{}\0{}\0{:?}\0{}",
        test,
        selector.check.as_deref().unwrap_or(""),
        selector.phase,
        selector.case_id.as_deref().unwrap_or("")
    )
}

fn retention_test_key(test: &str, complete: bool) -> String {
    if complete {
        test.to_owned()
    } else {
        // Keep crash-residue depth accounting separate so incomplete attempts
        // cannot evict sealed evidence for the same declared test identity.
        format!("{test}\0abandoned-incomplete")
    }
}

fn retention_history_key(test: &str, selector: &LeafSelector, complete: bool) -> String {
    history_key(&retention_test_key(test, complete), selector)
}

fn run_retention_epoch_ms(run: &InventoryRun) -> Option<u64> {
    (!run.active).then(|| {
        run.metadata
            .finished_at_epoch_ms
            .unwrap_or(run.metadata.started_at_epoch_ms)
    })
}

fn leaf_retention_epoch_ms(run: &InventoryRun, leaf: &InventoryLeaf) -> Option<u64> {
    (!leaf.active).then(|| {
        leaf.metadata
            .finished_at_epoch_ms
            .or(run.metadata.finished_at_epoch_ms)
            .unwrap_or(leaf.metadata.started_at_epoch_ms)
    })
}

fn depth_ranks(runs: &[InventoryRun]) -> BTreeMap<(String, String), u64> {
    let mut groups = BTreeMap::<String, Vec<(&InventoryLeaf, u64)>>::new();
    for run in runs {
        for leaf in &run.leaves {
            if let Some(retention_at) = leaf_retention_epoch_ms(run, leaf) {
                groups
                    .entry(retention_history_key(
                        &leaf.test,
                        &leaf.selector,
                        leaf.metadata.complete,
                    ))
                    .or_default()
                    .push((leaf, retention_at));
            }
        }
    }
    let mut output = BTreeMap::new();
    for (key, leaves) in &mut groups {
        leaves.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| right.0.run_id.cmp(&left.0.run_id))
        });
        for (index, (leaf, _)) in leaves.iter().enumerate() {
            output.insert(
                (leaf.run_id.clone(), key.clone()),
                u64::try_from(index + 1).unwrap_or(u64::MAX),
            );
        }
    }
    output
}

fn executor_depth_rank(
    runs: &[InventoryRun],
    target_run: &str,
    target_test: &str,
    target_complete: bool,
) -> Option<u64> {
    let target_key = retention_test_key(target_test, target_complete);
    let mut retained: Vec<(&InventoryRun, u64)> = runs
        .iter()
        .filter_map(|run| {
            let retention_at = run_retention_epoch_ms(run)?;
            (retention_test_key(&run.metadata.test, run.metadata.complete) == target_key
                && !run.executor_streams.is_empty())
            .then_some((run, retention_at))
        })
        .collect();
    retained.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.0.metadata.run_id.cmp(&left.0.metadata.run_id))
    });
    retained
        .iter()
        .position(|(run, _)| run.metadata.run_id == target_run)
        .map(|index| u64::try_from(index + 1).unwrap_or(u64::MAX))
}

fn log_ref(run_id: &str, selector: &LeafSelector, stream: LogStream) -> LogRef {
    LogRef {
        run_id: run_id.to_owned(),
        check: selector.check.clone(),
        phase: selector.phase,
        case: selector.case_id.clone(),
        stream,
    }
}

fn relative_leaf_path(run_id: &str, components: &[String]) -> PathBuf {
    let mut path = PathBuf::from(run_id);
    for component in components.iter().skip(1) {
        path.push(component);
    }
    path
}

fn query_digest(request: &LogQueryRequest, run_id: &str) -> Result<String, LogQueryError> {
    let mut normalized = request.clone();
    normalized.selector.run_id = Some(run_id.to_owned());
    normalized.options.cursor = None;
    let encoded = serde_json::to_vec(&normalized).map_err(|_| LogQueryError::Unavailable)?;
    Ok(lower_hex(&Sha256::digest(encoded)))
}

fn content_cursor(
    request: &LogQueryRequest,
    selected: &SelectedStream,
    digest: &str,
) -> Result<Option<CursorPayload>, LogQueryError> {
    let Some(encoded) = request.options.cursor.as_deref() else {
        return Ok(None);
    };
    let cursor = decode_cursor(encoded)?;
    if cursor.schema != 2
        || cursor.operation != request.operation
        || cursor.log_ref.as_ref() != Some(&selected.log_ref)
        || cursor.run_id != selected.log_ref.run_id
        || cursor.device != selected.device
        || cursor.inode != selected.inode
        || cursor.snapshot_bytes
            > selected
                .file
                .metadata()
                .map_err(|_| LogQueryError::Unavailable)?
                .len()
        || cursor.snapshot_lines > selected.metadata.lines
        || cursor.query_sha256 != digest
    {
        return Err(LogQueryError::CursorStale);
    }
    Ok(Some(cursor))
}

fn clamp_to_cursor_snapshot(
    selected: &mut SelectedStream,
    cursor: Option<&CursorPayload>,
) -> Result<(), LogQueryError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor.snapshot_bytes > selected.snapshot_bytes
        || cursor.snapshot_lines > selected.metadata.lines
    {
        return Err(LogQueryError::CursorStale);
    }
    selected.snapshot_bytes = cursor.snapshot_bytes;
    selected.metadata.bytes = cursor.snapshot_bytes;
    selected.metadata.lines = cursor.snapshot_lines;
    Ok(())
}

fn make_content_cursor(
    request: &LogQueryRequest,
    selected: &SelectedStream,
    digest: &str,
    position: u64,
    byte_position: u64,
) -> Result<String, LogQueryError> {
    make_content_cursor_with_state(
        request,
        selected,
        digest,
        position,
        byte_position,
        0,
        0,
        None,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_content_cursor_with_state(
    request: &LogQueryRequest,
    selected: &SelectedStream,
    digest: &str,
    position: u64,
    byte_position: u64,
    auxiliary_position: u64,
    auxiliary_byte_position: u64,
    pending_line_end: Option<u64>,
    emitted_through_line: u64,
) -> Result<String, LogQueryError> {
    encode_cursor(&CursorPayload {
        schema: 2,
        operation: request.operation,
        log_ref: Some(selected.log_ref.clone()),
        run_id: selected.log_ref.run_id.clone(),
        device: selected.device,
        inode: selected.inode,
        snapshot_bytes: selected.snapshot_bytes,
        snapshot_lines: selected.metadata.lines,
        position,
        byte_position,
        auxiliary_position,
        auxiliary_byte_position,
        pending_line_end,
        emitted_through_line,
        query_sha256: digest.to_owned(),
    })
}

fn verify_catalog_cursor(
    cursor: &CursorPayload,
    operation: LogQueryOperation,
    run_id: &str,
    identity: (u64, u64),
    entries: u64,
    digest: &str,
) -> Result<(), LogQueryError> {
    if cursor.schema != 2
        || cursor.operation != operation
        || cursor.log_ref.is_some()
        || cursor.run_id != run_id
        || (cursor.device, cursor.inode) != identity
        || cursor.snapshot_bytes != entries
        || cursor.snapshot_lines != 0
        || cursor.byte_position != 0
        || cursor.auxiliary_position != 0
        || cursor.auxiliary_byte_position != 0
        || cursor.pending_line_end.is_some()
        || cursor.emitted_through_line != 0
        || cursor.query_sha256 != digest
    {
        return Err(LogQueryError::CursorStale);
    }
    Ok(())
}

fn encode_cursor(cursor: &CursorPayload) -> Result<String, LogQueryError> {
    let payload = serde_json::to_vec(cursor).map_err(|_| LogQueryError::Unavailable)?;
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(&payload);
    let checksum = hasher.finalize();
    let encoded = format!(
        "{}.{}",
        base64_url_encode(&payload),
        base64_url_encode(&checksum)
    );
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(LogQueryError::Unavailable);
    }
    Ok(encoded)
}

fn decode_cursor(encoded: &str) -> Result<CursorPayload, LogQueryError> {
    if encoded.is_empty() || encoded.len() > MAX_CURSOR_BYTES {
        return Err(LogQueryError::CursorStale);
    }
    let (payload, checksum) = encoded.split_once('.').ok_or(LogQueryError::CursorStale)?;
    let payload = base64_url_decode(payload).ok_or(LogQueryError::CursorStale)?;
    let checksum = base64_url_decode(checksum).ok_or(LogQueryError::CursorStale)?;
    let mut hasher = Sha256::new();
    hasher.update(CURSOR_DOMAIN);
    hasher.update(&payload);
    if checksum.as_slice() != hasher.finalize().as_slice() {
        return Err(LogQueryError::CursorStale);
    }
    serde_json::from_slice(&payload).map_err(|_| LogQueryError::CursorStale)
}

fn current_run_id(worktree: &Path) -> Result<String, LogQueryError> {
    let root = open_directory_path(worktree)?;
    let devcoordinator = open_dir(&root, ".devcoordinator")?;
    let test = open_dir(&devcoordinator, "test")?;
    let current = open_dir(&test, "current")?;
    let value: serde_json::Value = read_json(&current, "summary.json", 2 * 1024 * 1024)?;
    let run_id = value
        .get("run_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(LogQueryError::StoreMalformed)?
        .to_owned();
    validate_run_id(&run_id)?;
    Ok(run_id)
}

fn open_directory_path(path: &Path) -> Result<File, LogQueryError> {
    unix_fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(map_open_error)
}

fn open_dir(parent: &File, name: &str) -> Result<File, LogQueryError> {
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(map_open_error)
}

fn ensure_dir(parent: &File, name: &str) -> Result<File, LogQueryError> {
    match unix_fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(_) => return Err(LogQueryError::Unavailable),
    }
    open_dir(parent, name)
}

fn open_file(parent: &File, name: &str) -> Result<File, LogQueryError> {
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(map_open_error)
}

fn open_or_create_file(parent: &File, name: &str) -> Result<File, LogQueryError> {
    unix_fs::openat(
        parent,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(map_open_error)
}

fn open_leaf_components(run: &File, components: &[String]) -> Result<File, LogQueryError> {
    let mut current = run.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    for component in components {
        current = open_dir(&current, component)?;
    }
    Ok(current)
}

fn map_open_error(error: rustix::io::Errno) -> LogQueryError {
    if error == rustix::io::Errno::NOENT {
        LogQueryError::LogNotFound
    } else if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        LogQueryError::StoreMalformed
    } else {
        LogQueryError::Unavailable
    }
}

fn directory_names(directory: &File) -> Result<Vec<String>, LogQueryError> {
    let mut reader = Dir::read_from(directory).map_err(|_| LogQueryError::Unavailable)?;
    let mut names = Vec::new();
    for entry in &mut reader {
        let entry = entry.map_err(|_| LogQueryError::Unavailable)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::str::from_utf8(bytes)
            .map_err(|_| LogQueryError::StoreMalformed)?
            .to_owned();
        if name.is_empty() || name.contains('/') || name.as_bytes().contains(&0) {
            return Err(LogQueryError::StoreMalformed);
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn read_json<T: for<'de> Deserialize<'de>>(
    directory: &File,
    name: &str,
    maximum: u64,
) -> Result<T, LogQueryError> {
    let file = open_file(directory, name)?;
    let details = file.metadata().map_err(|_| LogQueryError::Unavailable)?;
    if !details.is_file() || details.len() > maximum {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut payload = Vec::with_capacity(details.len() as usize);
    file.take(maximum + 1)
        .read_to_end(&mut payload)
        .map_err(|_| LogQueryError::Unavailable)?;
    if payload.len() as u64 > maximum {
        return Err(LogQueryError::StoreMalformed);
    }
    serde_json::from_slice(&payload).map_err(|_| LogQueryError::StoreMalformed)
}

fn run_is_locked(run_dir: &File) -> Result<bool, LogQueryError> {
    let lock = open_file(run_dir, "active.lock")?;
    match unix_fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(error)
            if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::WOULDBLOCK =>
        {
            Ok(true)
        }
        Err(_) => Err(LogQueryError::Unavailable),
    }
}

fn lock_and_revalidate_victim(
    runs: &File,
    location: &RetentionLocation,
    active_run_id: Option<&str>,
) -> Result<Option<File>, LogQueryError> {
    if active_run_id == Some(location.run_id.as_str()) {
        return Ok(None);
    }
    let run = required_store_entry(open_dir(runs, &location.run_id))?;
    if unix_fs::statat(&run, "finalization.pending", AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Ok(None);
    }
    if file_identity(&run)? != location.run_identity {
        return Err(LogQueryError::StoreMalformed);
    }
    let lock = required_store_entry(open_file(&run, "active.lock"))?;
    match unix_fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error)
            if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::WOULDBLOCK =>
        {
            return Ok(None);
        }
        Err(_) => return Err(LogQueryError::Unavailable),
    }
    let leaf = required_store_entry(open_leaf_components(&run, &location.components[1..]))?;
    if file_identity(&leaf)? != location.leaf_identity {
        return Err(LogQueryError::StoreMalformed);
    }
    Ok(Some(lock))
}

fn file_identity(file: &File) -> Result<(u64, u64), LogQueryError> {
    let metadata = file.metadata().map_err(|_| LogQueryError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn recover_garbage(garbage: &File) -> Result<u64, LogQueryError> {
    let names = directory_names(garbage)?;
    if names.iter().any(|name| !valid_garbage_name(name)) {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut removed = 0_u64;
    for name in names {
        delete_tree_at(garbage, &name)?;
        removed = removed.saturating_add(1);
    }
    garbage.sync_all().map_err(|_| LogQueryError::Unavailable)?;
    Ok(removed)
}

fn rename_leaf_to_garbage(
    runs: &File,
    garbage: &File,
    components: &[String],
    expected_identity: (u64, u64),
) -> Result<(), LogQueryError> {
    if components.len() < 2 {
        return Err(LogQueryError::StoreMalformed);
    }
    let mut parent = runs.try_clone().map_err(|_| LogQueryError::Unavailable)?;
    for component in &components[..components.len() - 1] {
        parent = open_dir(&parent, component)?;
    }
    let leaf = components.last().ok_or(LogQueryError::StoreMalformed)?;
    let opened_leaf = required_store_entry(open_dir(&parent, leaf))?;
    if file_identity(&opened_leaf)? != expected_identity {
        return Err(LogQueryError::StoreMalformed);
    }
    let identity = components.join("/");
    let digest = lower_hex(&Sha256::digest(identity.as_bytes()));
    let garbage_name = format!("g-{}-{:016x}", &digest[..16], epoch_ms());
    unix_fs::renameat(&parent, leaf, garbage, garbage_name.as_str())
        .map_err(|_| LogQueryError::Unavailable)?;
    let moved = required_store_entry(open_dir(garbage, &garbage_name))?;
    if file_identity(&moved)? != expected_identity {
        return Err(LogQueryError::StoreMalformed);
    }
    parent.sync_all().map_err(|_| LogQueryError::Unavailable)?;
    garbage.sync_all().map_err(|_| LogQueryError::Unavailable)?;
    delete_tree_at(garbage, &garbage_name)?;
    garbage.sync_all().map_err(|_| LogQueryError::Unavailable)?;
    Ok(())
}

fn delete_tree_at(parent: &File, name: &str) -> Result<(), LogQueryError> {
    let details =
        unix_fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_open_error)?;
    if FileType::from_raw_mode(details.st_mode) == FileType::Directory {
        let directory = open_dir(parent, name)?;
        for child in directory_names(&directory)? {
            delete_tree_at(&directory, &child)?;
        }
        directory
            .sync_all()
            .map_err(|_| LogQueryError::Unavailable)?;
        unix_fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
            .map_err(|_| LogQueryError::Unavailable)?;
    } else {
        unix_fs::unlinkat(parent, name, AtFlags::empty())
            .map_err(|_| LogQueryError::Unavailable)?;
    }
    Ok(())
}

fn valid_repository_id(value: &str) -> bool {
    value.len() == 17
        && value.starts_with('r')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_run_id(value: &str) -> Result<(), LogQueryError> {
    let bytes = value.as_bytes();
    let valid = (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(LogQueryError::ArgsInvalid)
    }
}

fn valid_check(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        })
}

fn valid_case(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_garbage_name(value: &str) -> bool {
    value.strip_prefix("g-").is_some_and(|rest| {
        rest.len() == 33
            && rest.as_bytes()[16] == b'-'
            && rest.bytes().enumerate().all(|(index, byte)| {
                index == 16 || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            })
    })
}

const fn stream_name(stream: LogStream) -> &'static str {
    match stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn iso_from_epoch_ms(epoch_ms: u64) -> String {
    let seconds = epoch_ms / 1_000;
    let milliseconds = epoch_ms % 1_000;
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    let (year, month, day) = civil_from_days(days as i128);
    if milliseconds == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z"
        )
    }
}

fn civil_from_days(days_since_epoch: i128) -> (i128, i128, i128) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    (year, month, day)
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn base64_standard_encode(input: &[u8]) -> String {
    base64_encode(
        input,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
    )
}

fn base64_url_encode(input: &[u8]) -> String {
    base64_encode(
        input,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
    )
    .trim_end_matches('=')
    .to_owned()
}

fn base64_encode(input: &[u8], alphabet: &[u8; 64]) -> String {
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(alphabet[(first >> 2) as usize] as char);
        output.push(alphabet[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(alphabet[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(alphabet[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    if input
        .bytes()
        .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'-' && byte != b'_')
    {
        return None;
    }
    let mut output = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).saturating_sub(1);
        }
    }
    if bits > 0 && accumulator != 0 {
        return None;
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::log_store::RunLogLease;
    use devcoordinator2_executor_protocol::{
        DiagnosticExit, DiagnosticOrigin, ErrorCategory, LeafStatus, RunStatus, SourceLocation,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temporary(label: &str) -> PathBuf {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "devcoordinator2-log-query-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join(".devcoordinator/test/logs/runs")).expect("store root");
        fs::create_dir_all(path.join(".devcoordinator/test/current")).expect("current root");
        path
    }

    fn run_id(sequence: u64) -> String {
        format!("t20260902T{sequence:06}Z-abcdef")
    }

    fn write_current(root: &Path, run_id: &str) {
        fs::write(
            root.join(".devcoordinator/test/current/summary.json"),
            format!("{{\"schema_version\":2,\"run_id\":\"{run_id}\"}}\n"),
        )
        .expect("current summary");
    }

    fn create_run(
        root: &Path,
        sequence: u64,
        case_id: &str,
        payload: &[u8],
        finished_at_ms: u64,
        active: bool,
    ) -> (String, Option<RunLogLease>) {
        let run_id = run_id(sequence);
        let directory = root.join(".devcoordinator/test/logs/runs").join(&run_id);
        fs::create_dir(&directory).expect("run directory");
        let lease = RunLogLease::acquire(&directory, &run_id).expect("lease");
        let selector = LeafSelector::case("unit", case_id).expect("selector");
        for (stream, content) in [
            (LogStream::Stdout, payload),
            (LogStream::Stderr, b"stderr evidence\n".as_slice()),
        ] {
            let mut writer = lease
                .create_stream(selector.clone(), stream)
                .expect("stream");
            writer.write_all(content).expect("complete write");
            writer.seal().expect("sealed stream");
        }
        fs::create_dir_all(
            directory
                .join("checks/unit/cases")
                .join(case_id)
                .join("diagnostics"),
        )
        .expect("diagnostics directory");
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
                started_at_epoch_ms: finished_at_ms.saturating_sub(100),
                finished_at_epoch_ms: (!active).then_some(finished_at_ms),
                process_started: true,
                complete: !active,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .expect("leaf metadata");
        lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: run_id.clone(),
                test: "complete".into(),
                started_at_epoch_ms: finished_at_ms.saturating_sub(200),
                finished_at_epoch_ms: (!active).then_some(finished_at_ms),
                status: if active {
                    RunStatus::Running
                } else {
                    RunStatus::Passed
                },
                complete: !active,
            })
            .expect("run metadata");
        write_current(root, &run_id);
        if active {
            (run_id, Some(lease))
        } else {
            drop(lease);
            (run_id, None)
        }
    }

    fn create_abandoned_run(
        root: &Path,
        sequence: u64,
        payload: &[u8],
        started_at_ms: u64,
    ) -> String {
        let run_id = run_id(sequence);
        let directory = root.join(".devcoordinator/test/logs/runs").join(&run_id);
        fs::create_dir(&directory).expect("run directory");
        let lease = RunLogLease::acquire(&directory, &run_id).expect("lease");
        lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: run_id.clone(),
                test: "complete".into(),
                started_at_epoch_ms: started_at_ms,
                finished_at_epoch_ms: None,
                status: RunStatus::Running,
                complete: false,
            })
            .expect("running metadata");
        let selector = LeafSelector::case("unit", "case-1").expect("selector");
        let mut stdout = lease
            .create_stream(selector, LogStream::Stdout)
            .expect("stdout");
        stdout.write_all(payload).expect("partial output");
        drop(stdout);
        let leaf = directory.join("checks/unit/cases/case-1");
        fs::remove_file(leaf.join("stdout.meta.json")).expect("remove unsealed metadata");
        drop(lease);
        run_id
    }

    fn request(
        operation: LogQueryOperation,
        run_id: &str,
        options: LogQueryOptions,
    ) -> LogQueryRequest {
        LogQueryRequest {
            schema: 2,
            operation,
            repository_id: "raaaaaaaaaaaaaaaa".into(),
            selector: LogQuerySelector {
                run_id: Some(run_id.into()),
                check: Some("unit".into()),
                phase: Some(LogPhase::Case),
                case_id: Some("case-1".into()),
                stream: Some(LogStream::Stdout),
            },
            options,
        }
    }

    fn content(result: LogQueryResult) -> ContentResult {
        match result {
            LogQueryResult::Content(result) => result,
            _ => panic!("expected content result"),
        }
    }

    #[test]
    fn catalogue_defaults_to_current_and_contains_no_stream_text_or_absolute_path() {
        let root = temporary("catalog");
        let now = epoch_ms();
        let (run, _) = create_run(&root, 1, "case-1", b"private sentinel\n", now, false);
        let mut query = request(LogQueryOperation::Catalog, &run, LogQueryOptions::default());
        query.selector.run_id = None;
        query.selector.phase = None;
        query.selector.case_id = None;
        query.selector.stream = None;
        query.options.limit = Some(1);
        let result = execute_log_query(&root, query.clone()).expect("catalogue");
        let LogQueryResult::Catalog(catalogue) = result else {
            panic!("catalogue result")
        };
        assert_eq!(catalogue.entries.len(), 1);
        let stdout = catalogue
            .entries
            .iter()
            .find(|entry| entry.log_ref.stream == LogStream::Stdout)
            .expect("stdout");
        assert_eq!(stdout.bytes, 17);
        assert_eq!(stdout.lines, Some(1));
        assert!(stdout.complete && !stdout.truncated);
        assert_eq!(stdout.depth_rank, Some(1));
        assert!(stdout.expires_at.is_some());
        let encoded = serde_json::to_string(&catalogue).expect("encode");
        assert!(!encoded.contains("private sentinel"));
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        query.options.cursor = catalogue.next_cursor;
        let LogQueryResult::Catalog(second) =
            execute_log_query(&root, query).expect("second catalogue page")
        else {
            panic!("catalogue result")
        };
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].log_ref.stream, LogStream::Stderr);
        assert!(second.next_cursor.is_none());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn exact_catalogue_does_not_exhaust_descriptors_on_large_history() {
        let root = temporary("catalog-large-history");
        let selected = create_run(&root, 1, "case-1", b"selected\n", epoch_ms(), false).0;
        for index in 2..=1100 {
            let _ = create_run(&root, index, "case-1", b"history\n", epoch_ms(), false);
        }
        let result = execute_log_query(
            &root,
            request(
                LogQueryOperation::Catalog,
                &selected,
                LogQueryOptions::default(),
            ),
        )
        .expect("catalogue large history");
        let LogQueryResult::Catalog(catalogue) = result else {
            panic!("catalogue result")
        };
        assert_eq!(catalogue.entries.len(), 1);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn tail_reaches_the_exact_end_beyond_the_former_four_megabyte_limit() {
        let root = temporary("large-tail");
        let mut payload = vec![b'x'; 5 * 1024 * 1024];
        payload.extend_from_slice(b"\nFINAL-LINE\n");
        let (run, _) = create_run(&root, 2, "case-1", &payload, epoch_ms(), false);
        let result = content(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Tail,
                    &run,
                    LogQueryOptions {
                        lines: Some(2),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                ),
            )
            .expect("tail"),
        );
        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].line_start, 1);
        assert_eq!(result.segments[0].line_end, 2);
        assert!(
            result.segments[0]
                .text
                .as_deref()
                .is_some_and(|text| text.ends_with("\nFINAL-LINE\n"))
        );
        assert_eq!(result.segments[0].byte_end, payload.len() as u64);
        assert_eq!(result.segments[0].byte_start, payload.len() as u64 - 1024);
        let continuation = request(
            LogQueryOperation::Tail,
            &run,
            LogQueryOptions {
                cursor: result.next_cursor.clone(),
                lines: Some(2),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        );
        let earlier =
            content(execute_log_query(&root, continuation.clone()).expect("earlier tail fragment"));
        assert_eq!(earlier.segments[0].byte_end, result.segments[0].byte_start);
        assert_eq!(earlier.segments[0].line_start, 1);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn active_executor_streams_are_computed_without_invented_metadata() {
        let root = temporary("executor-active");
        let now = epoch_ms();
        let (run, lease) = create_run(&root, 31, "case-1", b"leaf\n", now, true);
        let executor = root
            .join(".devcoordinator/test/logs/runs")
            .join(&run)
            .join("executor");
        fs::create_dir(&executor).expect("executor directory");
        fs::write(executor.join("stdout.log"), b"wrapper one\nwrapper two\n")
            .expect("wrapper output");
        fs::write(executor.join("stderr.log"), b"").expect("wrapper error");
        let query = LogQueryRequest {
            schema: 2,
            operation: LogQueryOperation::Catalog,
            repository_id: "raaaaaaaaaaaaaaaa".into(),
            selector: LogQuerySelector {
                run_id: Some(run.clone()),
                check: None,
                phase: Some(LogPhase::Executor),
                case_id: None,
                stream: None,
            },
            options: LogQueryOptions {
                limit: Some(100),
                ..LogQueryOptions::default()
            },
        };
        let LogQueryResult::Catalog(catalogue) =
            execute_log_query(&root, query).expect("catalogue")
        else {
            panic!("catalogue")
        };
        assert_eq!(catalogue.entries.len(), 2);
        let stdout = catalogue
            .entries
            .iter()
            .find(|entry| entry.log_ref.stream == LogStream::Stdout)
            .expect("stdout");
        assert_eq!(stdout.lines, Some(2));
        assert!(!stdout.complete);
        assert!(stdout.sha256.is_none());
        assert!(stdout.first_byte_at.is_none() && stdout.last_byte_at.is_none());
        let tail = content(
            execute_log_query(
                &root,
                LogQueryRequest {
                    schema: 2,
                    operation: LogQueryOperation::Tail,
                    repository_id: "raaaaaaaaaaaaaaaa".into(),
                    selector: LogQuerySelector {
                        run_id: Some(run.clone()),
                        check: None,
                        phase: Some(LogPhase::Executor),
                        case_id: None,
                        stream: Some(LogStream::Stdout),
                    },
                    options: LogQueryOptions {
                        lines: Some(1),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                },
            )
            .expect("tail executor"),
        );
        assert_eq!(tail.segments[0].text.as_deref(), Some("wrapper two\n"));
        drop(lease);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn active_check_streams_are_visible_before_leaf_and_stream_metadata_seal() {
        let root = temporary("active-check");
        let run = run_id(33);
        let run_dir = root.join(".devcoordinator/test/logs/runs").join(&run);
        fs::create_dir(&run_dir).expect("run directory");
        let lease = RunLogLease::acquire(&run_dir, &run).expect("lease");
        lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: run.clone(),
                test: "complete".into(),
                started_at_epoch_ms: epoch_ms(),
                finished_at_epoch_ms: None,
                status: RunStatus::Running,
                complete: false,
            })
            .expect("run metadata");
        let selector = LeafSelector::case("unit", "case-1").expect("selector");
        let mut stdout = lease
            .create_stream(selector.clone(), LogStream::Stdout)
            .expect("stdout");
        let stderr = lease
            .create_stream(selector, LogStream::Stderr)
            .expect("stderr");
        stdout
            .write_all(b"still running\nsecond line\n")
            .expect("active output");
        let query = request(
            LogQueryOperation::Catalog,
            &run,
            LogQueryOptions {
                limit: Some(100),
                ..LogQueryOptions::default()
            },
        );
        let LogQueryResult::Catalog(catalogue) =
            execute_log_query(&root, query).expect("catalogue")
        else {
            panic!("catalogue")
        };
        assert_eq!(catalogue.entries.len(), 1);
        assert_eq!(catalogue.entries[0].lines, Some(2));
        assert!(!catalogue.entries[0].complete);
        assert!(catalogue.entries[0].sha256.is_none());
        let tail = content(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Tail,
                    &run,
                    LogQueryOptions {
                        lines: Some(1),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                ),
            )
            .expect("active tail"),
        );
        assert_eq!(tail.segments[0].text.as_deref(), Some("second line\n"));
        drop(stderr);
        drop(stdout);
        drop(lease);
        fs::remove_dir_all(root).expect("cleanup");
    }

    // UIL-TESTING-LOGS-001: an abandoned partial neighbor must neither hide
    // healthy evidence nor prevent bounded access to the bytes it did retain.
    #[test]
    fn exact_queries_isolate_unlocked_incomplete_neighbors_and_expose_safe_partial_bytes() {
        let root = temporary("abandoned-neighbor");
        let now = epoch_ms();
        let (terminal, _) = create_run(&root, 43, "case-1", b"terminal\n", now, false);
        let abandoned = create_abandoned_run(
            &root,
            44,
            b"partial first\npartial second\n",
            now.saturating_sub(10_000),
        );

        let LogQueryResult::Catalog(terminal_catalogue) = execute_log_query(
            &root,
            request(
                LogQueryOperation::Catalog,
                &terminal,
                LogQueryOptions::default(),
            ),
        )
        .expect("terminal catalogue despite abandoned neighbor") else {
            panic!("catalogue")
        };
        assert_eq!(terminal_catalogue.entries.len(), 1);
        assert!(
            terminal_catalogue
                .entries
                .iter()
                .all(|entry| entry.complete)
        );

        let LogQueryResult::Catalog(partial_catalogue) = execute_log_query(
            &root,
            request(
                LogQueryOperation::Catalog,
                &abandoned,
                LogQueryOptions::default(),
            ),
        )
        .expect("abandoned partial catalogue") else {
            panic!("catalogue")
        };
        assert_eq!(partial_catalogue.entries.len(), 1);
        assert_eq!(partial_catalogue.entries[0].bytes, 29);
        assert_eq!(partial_catalogue.entries[0].lines, Some(2));
        assert!(!partial_catalogue.entries[0].complete);
        assert!(partial_catalogue.entries[0].sha256.is_none());
        assert!(partial_catalogue.entries[0].expires_at.is_some());
        assert_eq!(partial_catalogue.entries[0].depth_rank, Some(1));

        let partial_tail = content(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Tail,
                    &abandoned,
                    LogQueryOptions {
                        lines: Some(1),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                ),
            )
            .expect("abandoned partial tail"),
        );
        assert_eq!(
            partial_tail.segments[0].text.as_deref(),
            Some("partial second\n")
        );

        let partial_metadata = root
            .join(".devcoordinator/test/logs/runs")
            .join(&abandoned)
            .join("checks/unit/cases/case-1/stdout.meta.json");
        fs::write(&partial_metadata, b"{}\n").expect("malformed partial metadata");
        assert_eq!(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Catalog,
                    &abandoned,
                    LogQueryOptions::default(),
                ),
            ),
            Err(LogQueryError::StoreMalformed)
        );
        fs::remove_file(&partial_metadata).expect("restore unsealed partial metadata");

        let terminal_stream = root
            .join(".devcoordinator/test/logs/runs")
            .join(&terminal)
            .join("checks/unit/cases/case-1/stdout.log");
        fs::write(&terminal_stream, b"tampered\n").expect("tamper selected stream");
        assert_eq!(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Catalog,
                    &terminal,
                    LogQueryOptions::default(),
                ),
            ),
            Err(LogQueryError::StoreMalformed)
        );

        let (active, active_lease) = create_run(&root, 45, "case-1", b"active\n", now, true);
        let mut active_request = request(
            LogQueryOperation::Catalog,
            &active,
            LogQueryOptions::default(),
        );
        active_request.selector.run_id = None;
        let LogQueryResult::Catalog(active_catalogue) = execute_log_query(&root, active_request)
            .expect("active catalogue despite malformed and abandoned neighbors")
        else {
            panic!("catalogue")
        };
        assert_eq!(active_catalogue.entries.len(), 1);
        assert_eq!(active_catalogue.entries[0].bytes, 7);
        drop(active_lease);
        fs::remove_dir_all(root).expect("cleanup");
    }

    // UIL-TESTING-LOGS-001: unlocked incomplete data is age/depth managed,
    // while a genuinely active run remains protected by its lease.
    #[test]
    fn retention_reclaims_abandoned_partial_leaves_without_touching_active_data() {
        let root = temporary("abandoned-retention");
        let now = epoch_ms();
        let mut abandoned = Vec::new();
        for (sequence, age_ms) in [(46, 4_000), (48, 3_000), (49, 2_000), (50, 1_000)] {
            abandoned.push(create_abandoned_run(
                &root,
                sequence,
                b"abandoned bytes\n",
                now.saturating_sub(age_ms),
            ));
        }
        let (active, active_lease) = create_run(
            &root,
            47,
            "case-1",
            b"active bytes\n",
            now.saturating_sub(10_000),
            true,
        );
        let result = prune_logs(
            &root,
            LogPruneRequest {
                schema: 2,
                repository_id: "raaaaaaaaaaaaaaaa".into(),
                max_age_seconds: 100,
                case_depth: 3,
                active_run_id: Some(active.clone()),
            },
        )
        .expect("depth-reclaim abandoned evidence");
        assert_eq!(result.removed_leaf_folders, 1);
        assert_eq!(result.retained_active, 1);
        assert!(
            !root
                .join(".devcoordinator/test/logs/runs")
                .join(&abandoned[0])
                .join("checks/unit/cases/case-1")
                .exists()
        );
        assert!(
            root.join(".devcoordinator/test/logs/runs")
                .join(&active)
                .join("checks/unit/cases/case-1/stdout.log")
                .is_file()
        );
        let age_result = prune_logs(
            &root,
            LogPruneRequest {
                schema: 2,
                repository_id: "raaaaaaaaaaaaaaaa".into(),
                max_age_seconds: 1,
                case_depth: 100,
                active_run_id: Some(active.clone()),
            },
        )
        .expect("age-reclaim remaining abandoned evidence");
        assert_eq!(age_result.removed_leaf_folders, 3);
        assert_eq!(age_result.retained_active, 1);
        assert!(abandoned.iter().all(|run| {
            !root
                .join(".devcoordinator/test/logs/runs")
                .join(run)
                .join("checks/unit/cases/case-1")
                .exists()
        }));
        assert!(
            root.join(".devcoordinator/test/logs/runs")
                .join(&active)
                .join("checks/unit/cases/case-1/stdout.log")
                .is_file()
        );
        drop(active_lease);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn literal_search_does_not_treat_metacharacters_as_a_pattern_and_pages() {
        let root = temporary("search");
        let payload = b"before\n[literal].* first\nnoise\n[literal].* second\nafter\n";
        let (run, _) = create_run(&root, 3, "case-1", payload, epoch_ms(), false);
        let mut query = request(
            LogQueryOperation::Search,
            &run,
            LogQueryOptions {
                text: Some("[literal].*".into()),
                max_matches: Some(1),
                context_lines: Some(0),
                max_bytes: Some(4096),
                ..LogQueryOptions::default()
            },
        );
        let first = match execute_log_query(&root, query.clone()).expect("first search") {
            LogQueryResult::Search(result) => result,
            _ => panic!("search result"),
        };
        assert_eq!(first.matches.len(), 1);
        assert_eq!(first.matches[0].line_start, 2);
        let cursor = first.next_cursor.expect("next cursor");
        query.options.cursor = Some(cursor);
        let second = match execute_log_query(&root, query).expect("second search") {
            LogQueryResult::Search(result) => result,
            _ => panic!("search result"),
        };
        assert_eq!(second.matches[0].line_start, 4);
        assert!(second.next_cursor.is_none());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn exact_line_and_binary_byte_ranges_are_bounded_and_addressed() {
        let root = temporary("ranges");
        let (run, _) = create_run(&root, 4, "case-1", b"one\ntwo\nthree\n", epoch_ms(), false);
        let line_result = content(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Range,
                    &run,
                    LogQueryOptions {
                        line_start: Some(2),
                        line_end: Some(2),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                ),
            )
            .expect("line range"),
        );
        assert_eq!(line_result.segments[0].text.as_deref(), Some("two\n"));
        assert_eq!(
            (
                line_result.segments[0].byte_start,
                line_result.segments[0].byte_end
            ),
            (4, 8)
        );

        let stream = root
            .join(".devcoordinator/test/logs/runs")
            .join(&run)
            .join("checks/unit/cases/case-1/stdout.log");
        let metadata = stream.with_file_name("stdout.meta.json");
        fs::write(&stream, [0xff, 0x00, 0x01, 0x02]).expect("binary stream");
        let mut value: StreamMetadata =
            serde_json::from_slice(&fs::read(&metadata).expect("metadata")).expect("decode");
        value.bytes = 4;
        value.lines = 1;
        value.sha256 = lower_hex(&Sha256::digest([0xff, 0x00, 0x01, 0x02]));
        fs::write(&metadata, serde_json::to_vec(&value).expect("encode")).expect("metadata");
        let byte_result = content(
            execute_log_query(
                &root,
                request(
                    LogQueryOperation::Range,
                    &run,
                    LogQueryOptions {
                        byte_start: Some(0),
                        byte_end: Some(4),
                        max_bytes: Some(1024),
                        ..LogQueryOptions::default()
                    },
                ),
            )
            .expect("byte range"),
        );
        assert_eq!(byte_result.segments[0].base64.as_deref(), Some("/wABAg=="));
        assert!(byte_result.segments[0].text.is_none());
        assert!(serde_json::to_vec(&byte_result).expect("result").len() < 64 * 1024);

        let large_binary = vec![0xff; 48 * 1024];
        fs::write(&stream, &large_binary).expect("large binary stream");
        value.bytes = large_binary.len() as u64;
        value.sha256 = lower_hex(&Sha256::digest(&large_binary));
        fs::write(&metadata, serde_json::to_vec(&value).expect("encode")).expect("metadata");
        let mut large_query = request(
            LogQueryOperation::Range,
            &run,
            LogQueryOptions {
                byte_start: Some(0),
                byte_end: Some(48 * 1024),
                max_bytes: Some(48 * 1024),
                ..LogQueryOptions::default()
            },
        );
        let first = content(execute_log_query(&root, large_query.clone()).expect("first chunk"));
        assert_eq!(first.segments[0].byte_end, 32 * 1024);
        assert!(first.response_truncated);
        assert!(serde_json::to_vec(&first).expect("result").len() < 64 * 1024);
        large_query.options.cursor = first.next_cursor;
        let second = content(execute_log_query(&root, large_query).expect("second chunk"));
        assert_eq!(second.segments[0].byte_start, 32 * 1024);
        assert_eq!(second.segments[0].byte_end, 48 * 1024);
        assert!(!second.response_truncated);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn failure_context_uses_deterministic_priority_and_final_lines() {
        let root = temporary("failure-context");
        let payload = b"ordinary\nconsole.error network\n    at frame\npanicked at src/lib.rs\nerror[E0308]: mismatch\nassertion failed\nfinal detail\n";
        let (run, _) = create_run(&root, 5, "case-1", payload, epoch_ms(), false);
        let result = match execute_log_query(
            &root,
            request(
                LogQueryOperation::FailureContext,
                &run,
                LogQueryOptions {
                    limit: Some(20),
                    context_lines: Some(0),
                    max_bytes: Some(8192),
                    ..LogQueryOptions::default()
                },
            ),
        )
        .expect("failure context")
        {
            LogQueryResult::FailureContext(result) => result,
            _ => panic!("failure context result"),
        };
        let ranks: Vec<u8> = result.contexts.iter().filter_map(|row| row.rank).collect();
        assert!(ranks.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(ranks.first(), Some(&1));
        assert!(ranks.contains(&7));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn failure_context_prioritizes_validated_structured_diagnostics() {
        let root = temporary("structured-context");
        let (run, _) = create_run(&root, 32, "case-1", b"ordinary output\n", epoch_ms(), false);
        let leaf = root
            .join(".devcoordinator/test/logs/runs")
            .join(&run)
            .join("checks/unit/cases/case-1");
        let leaf_path = leaf.join("leaf.json");
        let mut leaf_metadata: LeafLogMetadata =
            serde_json::from_slice(&fs::read(&leaf_path).expect("leaf metadata"))
                .expect("decode leaf");
        leaf_metadata.structured_evidence_formats = vec![DiagnosticReportFormat::Junit];
        leaf_metadata.structured_evidence_count = 1;
        fs::write(
            &leaf_path,
            serde_json::to_vec(&leaf_metadata).expect("encode leaf"),
        )
        .expect("write leaf");
        let mut failure = FailureIndexEntry {
            check: Some("unit".into()),
            case: Some("case-1".into()),
            status: LeafStatus::Failed,
            exit: DiagnosticExit {
                code: Some(1),
                signal: None,
            },
            termination_reason: None,
            source: Some(SourceLocation {
                file: "src/parser.rs".into(),
                line: 81,
                column: Some(9),
            }),
            error_category: ErrorCategory::Assertion,
            expected: None,
            actual: None,
            fingerprint: String::new(),
            occurrences: 1,
            log_refs: vec![LogRef {
                run_id: run.clone(),
                check: Some("unit".into()),
                phase: LogPhase::Case,
                case: Some("case-1".into()),
                stream: LogStream::Stdout,
            }],
            origin: DiagnosticOrigin::Junit,
        };
        failure.fingerprint = crate::diagnostics::diagnostic_fingerprint(&failure);
        fs::write(
            leaf.join("diagnostics.json"),
            serde_json::to_vec(&StoredDiagnostics {
                schema: 2,
                check: Some("unit".into()),
                case_id: Some("case-1".into()),
                entries: vec![failure.clone()],
            })
            .expect("diagnostics"),
        )
        .expect("write diagnostics");
        let result = match execute_log_query(
            &root,
            request(
                LogQueryOperation::FailureContext,
                &run,
                LogQueryOptions {
                    limit: Some(20),
                    context_lines: Some(0),
                    max_bytes: Some(4096),
                    ..LogQueryOptions::default()
                },
            ),
        )
        .expect("failure context")
        {
            LogQueryResult::FailureContext(result) => result,
            _ => panic!("failure context result"),
        };
        assert_eq!(result.failures, vec![failure]);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cursor_rejects_replaced_stream_instead_of_reading_unrelated_bytes() {
        let root = temporary("cursor-stale");
        let payload = b"match\nmatch\n";
        let (run, _) = create_run(&root, 6, "case-1", payload, epoch_ms(), false);
        let mut query = request(
            LogQueryOperation::Search,
            &run,
            LogQueryOptions {
                text: Some("match".into()),
                max_matches: Some(1),
                context_lines: Some(0),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        );
        let first = match execute_log_query(&root, query.clone()).expect("search") {
            LogQueryResult::Search(result) => result,
            _ => panic!("search result"),
        };
        query.options.cursor = first.next_cursor;
        let stream = root
            .join(".devcoordinator/test/logs/runs")
            .join(&run)
            .join("checks/unit/cases/case-1/stdout.log");
        let replacement = stream.with_file_name("replacement");
        fs::write(&replacement, payload).expect("replacement");
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600))
            .expect("private replacement");
        fs::rename(&replacement, &stream).expect("replace");
        assert_eq!(
            execute_log_query(&root, query),
            Err(LogQueryError::CursorStale)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn prune_applies_depth_and_age_or_semantics_and_protects_locked_runs() {
        let root = temporary("prune");
        let now = epoch_ms();
        let mut runs = Vec::new();
        for sequence in 10..14 {
            runs.push(
                create_run(
                    &root,
                    sequence,
                    "case-1",
                    b"log\n",
                    now - (14 - sequence) * 1_000,
                    false,
                )
                .0,
            );
        }
        let (active_run, active_lease) =
            create_run(&root, 14, "case-1", b"active\n", now - 100_000, true);
        let result = prune_logs(
            &root,
            LogPruneRequest {
                schema: 2,
                repository_id: "raaaaaaaaaaaaaaaa".into(),
                max_age_seconds: 10_000,
                case_depth: 3,
                active_run_id: Some(active_run.clone()),
            },
        )
        .expect("depth prune");
        assert_eq!(result.removed_leaf_folders, 1);
        assert_eq!(result.retained_active, 1);
        assert!(
            !root
                .join(".devcoordinator/test/logs/runs")
                .join(&runs[0])
                .join("checks/unit/cases/case-1")
                .exists()
        );
        assert!(
            root.join(".devcoordinator/test/logs/runs")
                .join(&active_run)
                .join("checks/unit/cases/case-1")
                .exists()
        );
        let age = prune_logs(
            &root,
            LogPruneRequest {
                schema: 2,
                repository_id: "raaaaaaaaaaaaaaaa".into(),
                max_age_seconds: 1,
                case_depth: 100,
                active_run_id: Some(active_run.clone()),
            },
        )
        .expect("age prune");
        assert!(age.removed_leaf_folders >= 1);
        assert_eq!(age.retained_active, 1);
        drop(active_lease);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn garbage_recovery_and_symlink_attacks_never_follow_targets() {
        let root = temporary("nofollow");
        let (run, _) = create_run(&root, 20, "case-1", b"safe\n", epoch_ms(), false);
        let stream = root
            .join(".devcoordinator/test/logs/runs")
            .join(&run)
            .join("checks/unit/cases/case-1/stdout.log");
        let outside = root.join("outside");
        fs::write(&outside, b"must survive").expect("outside");
        fs::remove_file(&stream).expect("remove stream");
        symlink(&outside, &stream).expect("symlink");
        assert_eq!(
            execute_log_query(
                &root,
                request(LogQueryOperation::Catalog, &run, LogQueryOptions::default())
            ),
            Err(LogQueryError::StoreMalformed)
        );
        assert_eq!(fs::read(&outside).expect("outside"), b"must survive");
        fs::remove_file(&stream).expect("unlink attack");
        fs::write(&stream, b"safe\n").expect("restore stream");
        fs::set_permissions(&stream, fs::Permissions::from_mode(0o600))
            .expect("private restored stream");
        let garbage =
            root.join(".devcoordinator/test/logs/.garbage/g-aaaaaaaaaaaaaaaa-0000000000000001");
        fs::create_dir_all(&garbage).expect("garbage");
        fs::write(garbage.join("old"), b"old").expect("garbage file");
        let result = prune_logs(
            &root,
            LogPruneRequest {
                schema: 2,
                repository_id: "raaaaaaaaaaaaaaaa".into(),
                max_age_seconds: 10_000,
                case_depth: 100,
                active_run_id: None,
            },
        )
        .expect("recovery");
        assert_eq!(result.recovered_garbage, 1);
        assert!(!garbage.exists());
        assert_eq!(fs::read(&outside).expect("outside"), b"must survive");
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn malformed_requests_and_cursor_tampering_are_typed() {
        let root = temporary("bad-request");
        let (run, _) = create_run(&root, 30, "case-1", b"a\nb\n", epoch_ms(), false);
        let mut invalid = request(LogQueryOperation::Tail, &run, LogQueryOptions::default());
        invalid.repository_id = "../escape".into();
        assert_eq!(
            execute_log_query(&root, invalid),
            Err(LogQueryError::ArgsInvalid)
        );
        let mut query = request(
            LogQueryOperation::Search,
            &run,
            LogQueryOptions {
                text: Some("a".into()),
                max_matches: Some(1),
                context_lines: Some(0),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        );
        let result = match execute_log_query(&root, query.clone()).expect("search") {
            LogQueryResult::Search(result) => result,
            _ => panic!("search result"),
        };
        let mut cursor = result.next_cursor.unwrap_or_else(|| {
            make_content_cursor(
                &query,
                &select_stream(
                    &scan_store(&root, None, Some(&run))
                        .expect("inventory")
                        .runs
                        .into_iter()
                        .find(|entry| entry.metadata.run_id == run)
                        .expect("run"),
                    &query,
                    false,
                )
                .expect("selected"),
                &query_digest(&query, &run).expect("digest"),
                1,
                0,
            )
            .expect("cursor")
        });
        cursor.push('x');
        query.options.cursor = Some(cursor);
        assert_eq!(
            execute_log_query(&root, query),
            Err(LogQueryError::CursorStale)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn never_started_terminal_leaf_has_no_streams_but_started_leaf_requires_them() {
        let root = temporary("no-stream-leaf");
        let run = "run-local-no-stream".to_owned();
        let run_dir = root.join(".devcoordinator/test/logs/runs").join(&run);
        fs::create_dir(&run_dir).expect("run directory");
        let lease = RunLogLease::acquire(&run_dir, &run).expect("lease");
        lease
            .publish_leaf_metadata(&LeafLogMetadata {
                schema: 2,
                selector: LeafSelector::check("unit").expect("selector"),
                status: LeafStatus::Reused,
                exit: DiagnosticExit::default(),
                started_at_epoch_ms: 1,
                finished_at_epoch_ms: Some(1),
                process_started: false,
                complete: true,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .expect("leaf metadata");
        lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: run.clone(),
                test: "complete".into(),
                started_at_epoch_ms: 1,
                finished_at_epoch_ms: Some(2),
                status: RunStatus::Passed,
                complete: true,
            })
            .expect("run metadata");
        drop(lease);
        let mut catalog = request(LogQueryOperation::Catalog, &run, LogQueryOptions::default());
        catalog.selector.phase = Some(LogPhase::Check);
        catalog.selector.case_id = None;
        assert!(matches!(
            execute_log_query(&root, catalog),
            Ok(LogQueryResult::Catalog(CatalogResult { entries, .. })) if entries.is_empty()
        ));

        let leaf_path = run_dir.join("checks/unit/check/leaf.json");
        let mut metadata: LeafLogMetadata =
            serde_json::from_slice(&fs::read(&leaf_path).expect("leaf JSON")).expect("metadata");
        metadata.process_started = true;
        fs::write(&leaf_path, serde_json::to_vec(&metadata).expect("encode"))
            .expect("replace metadata");
        assert_eq!(
            execute_log_query(
                &root,
                request(LogQueryOperation::Catalog, &run, LogQueryOptions::default())
            ),
            Err(LogQueryError::StoreMalformed)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn tail_cursor_advances_across_newline_without_repeating_lines() {
        let root = temporary("tail-pages");
        let (run, _) = create_run(&root, 40, "case-1", b"aa\nbb\ncc\n", epoch_ms(), false);
        let mut query = request(
            LogQueryOperation::Tail,
            &run,
            LogQueryOptions {
                lines: Some(3),
                max_bytes: Some(3),
                ..LogQueryOptions::default()
            },
        );
        let mut lines = Vec::new();
        loop {
            let page = content(execute_log_query(&root, query.clone()).expect("tail page"));
            lines.push((
                page.segments[0].line_start,
                page.segments[0].text.clone().expect("text"),
            ));
            let Some(cursor) = page.next_cursor else {
                break;
            };
            query.options.cursor = Some(cursor);
        }
        assert_eq!(
            lines,
            vec![(3, "cc\n".into()), (2, "bb\n".into()), (1, "aa\n".into())]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn catalogue_case_phase_filter_enumerates_all_cases_for_a_check() {
        let root = temporary("catalog-cases");
        let now = epoch_ms();
        let (run, _) = create_run(&root, 44, "case-1", b"one\n", now, false);
        let run_dir = root.join(".devcoordinator/test/logs/runs").join(&run);
        let lease = RunLogLease::acquire(&run_dir, &run).expect("lease");
        let selector = LeafSelector::case("unit", "case-2").expect("selector");
        for stream in [LogStream::Stdout, LogStream::Stderr] {
            let mut writer = lease
                .create_stream(selector.clone(), stream)
                .expect("stream");
            writer.write_all(b"two\n").expect("write");
            writer.seal().expect("seal");
        }
        fs::create_dir_all(run_dir.join("checks/unit/cases/case-2/diagnostics"))
            .expect("diagnostics");
        lease
            .publish_leaf_metadata(&LeafLogMetadata {
                schema: 2,
                selector,
                status: LeafStatus::Passed,
                exit: DiagnosticExit::default(),
                started_at_epoch_ms: now.saturating_sub(1),
                finished_at_epoch_ms: Some(now),
                process_started: true,
                complete: true,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .expect("leaf metadata");
        drop(lease);
        let mut query = request(LogQueryOperation::Catalog, &run, LogQueryOptions::default());
        query.selector.case_id = None;
        let LogQueryResult::Catalog(result) =
            execute_log_query(&root, query).expect("case catalogue")
        else {
            panic!("catalogue")
        };
        let cases: BTreeSet<_> = result
            .entries
            .iter()
            .filter_map(|entry| entry.log_ref.case.as_deref())
            .collect();
        assert_eq!(cases, BTreeSet::from(["case-1", "case-2"]));

        let mut content = request(
            LogQueryOperation::Tail,
            &run,
            LogQueryOptions {
                lines: Some(1),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        );
        content.selector.case_id = None;
        assert_eq!(
            execute_log_query(&root, content),
            Err(LogQueryError::ArgsInvalid)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn search_cursor_finishes_a_long_matching_line_before_scanning_onward() {
        let root = temporary("search-pending");
        let mut payload = b"needle".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 9_000));
        payload.extend_from_slice(b"\nneedle\n");
        let (run, _) = create_run(&root, 41, "case-1", &payload, epoch_ms(), false);
        let mut query = request(
            LogQueryOperation::Search,
            &run,
            LogQueryOptions {
                text: Some("needle".into()),
                max_matches: Some(1),
                context_lines: Some(0),
                max_bytes: Some(4_096),
                ..LogQueryOptions::default()
            },
        );
        let mut expected_start = 0_u64;
        let mut saw_second_line = false;
        loop {
            let LogQueryResult::Search(page) =
                execute_log_query(&root, query.clone()).expect("search page")
            else {
                panic!("search result")
            };
            for segment in page.matches {
                if segment.line_start == 1 {
                    assert_eq!(segment.byte_start, expected_start);
                    expected_start = segment.byte_end;
                } else if segment.line_start == 2 {
                    saw_second_line = true;
                }
            }
            let Some(cursor) = page.next_cursor else {
                break;
            };
            query.options.cursor = Some(cursor);
        }
        assert_eq!(expected_start, 9_007);
        assert!(saw_second_line);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn failure_context_pages_ranked_items_by_index_without_looping() {
        let root = temporary("failure-pages");
        let payload = b"process exited 1\nordinary\nassertion failed\n";
        let (run, _) = create_run(&root, 42, "case-1", payload, epoch_ms(), false);
        let mut query = request(
            LogQueryOperation::FailureContext,
            &run,
            LogQueryOptions {
                limit: Some(1),
                context_lines: Some(0),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        );
        let LogQueryResult::FailureContext(first) =
            execute_log_query(&root, query.clone()).expect("first page")
        else {
            panic!("failure context")
        };
        assert_eq!(first.contexts[0].line_start, 3);
        query.options.cursor = first.next_cursor;
        let LogQueryResult::FailureContext(second) =
            execute_log_query(&root, query).expect("second page")
        else {
            panic!("failure context")
        };
        assert_eq!(second.contexts[0].line_start, 1);
        assert_ne!(
            first.contexts[0].fingerprint,
            second.contexts[0].fingerprint
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn escaped_search_output_is_paged_below_the_serialized_response_limit() {
        let root = temporary("escaped-budget");
        let mut payload = Vec::new();
        for _ in 0..100 {
            payload.extend(std::iter::repeat_n(0_u8, 300));
            payload.extend_from_slice(b"needle\n");
        }
        let (run, _) = create_run(&root, 43, "case-1", &payload, epoch_ms(), false);
        let LogQueryResult::Search(result) = execute_log_query(
            &root,
            request(
                LogQueryOperation::Search,
                &run,
                LogQueryOptions {
                    text: Some("needle".into()),
                    max_matches: Some(100),
                    context_lines: Some(0),
                    max_bytes: Some(MAX_QUERY_CONTENT_BYTES as u64),
                    ..LogQueryOptions::default()
                },
            ),
        )
        .expect("bounded escaped page") else {
            panic!("search result")
        };
        assert!(result.next_cursor.is_some());
        assert!(serde_json::to_vec(&result).expect("JSON").len() <= MAX_QUERY_RESULT_BYTES);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn prune_distinguishes_an_absent_store_from_a_corrupt_run() {
        let root = temporary("prune-missing");
        fs::remove_dir_all(root.join(".devcoordinator/test/logs")).expect("remove store");
        let request = LogPruneRequest {
            schema: 2,
            repository_id: "raaaaaaaaaaaaaaaa".into(),
            max_age_seconds: 86_400,
            case_depth: 3,
            active_run_id: None,
        };
        assert_eq!(
            prune_logs(&root, request.clone())
                .expect("absent store")
                .removed_leaf_folders,
            0
        );
        let runs = root.join(".devcoordinator/test/logs/runs");
        fs::create_dir_all(runs.join("corrupt-run")).expect("corrupt run");
        assert_eq!(
            prune_logs(&root, request),
            Err(LogQueryError::StoreMalformed)
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn epoch_format_and_base64_are_stable() {
        assert!(validate_run_id("run-local-1").is_ok());
        assert!(validate_run_id("skills-20260902T120000Z-123-abcdef").is_ok());
        assert!(validate_run_id("../escape").is_err());
        assert_eq!(iso_from_epoch_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            iso_from_epoch_ms(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        for value in [b"".as_slice(), b"a", b"ab", b"abc", b"cursor payload"] {
            let encoded = base64_url_encode(value);
            assert_eq!(base64_url_decode(&encoded).as_deref(), Some(value));
        }
    }
}
