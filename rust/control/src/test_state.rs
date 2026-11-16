//! Descriptor-relative governed-test state, summaries, history, and retry evidence.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use devcoordinator2_api::results::{
    LogCatalogReference, ProofKind as ApiProofKind, RunTerminationReason, TestStatus, TestSummary,
};
use devcoordinator2_api::test_containers::TestContainerRecord;
use devcoordinator2_executor_protocol::{
    ArtifactReceipt, CheckReport, DiagnosticExit, ExecutionPlan, ExecutionReport, LogStreamSummary,
    ProofKind, RunStatus, Schema2, ValidationTier,
};
use rustix::fs::{self as unix_fs, AtFlags, Dir, FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PLAN_FILE: &str = "check-plan.json";
pub const REPORT_FILE: &str = "check-report.json";
pub const SUMMARY_FILE: &str = "summary.json";
pub const ENV_FILE: &str = "env";
pub const CONTAINERS_FILE: &str = "containers.json";
pub const CONTAINER_LIFECYCLE_FILE: &str = "container-lifecycle.json";
const CONTAINER_CREATION_CLOSED_FILE: &str = "container-creation-closed.json";
const HISTORY_FILE: &str = "history.json";
const EVIDENCE_FILE: &str = "evidence.json";
const JSON_LIMIT: u64 = 2 * 1024 * 1024;
const HISTORY_CAP: usize = 1_000;
const EVIDENCE_CAP: usize = 50;
pub(crate) const CONTAINER_DIAGNOSTIC_CAP: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum TestStateError {
    #[error("container ownership metadata is busy")]
    Busy,
    #[error("invalid governed-test state: {0}")]
    Invalid(String),
    #[error("governed-test filesystem operation failed: {0}")]
    Filesystem(String),
    #[error("governed-test JSON is invalid: {0}")]
    Json(String),
}

pub struct PreparedRun {
    pub current_path: PathBuf,
    pub log_path: PathBuf,
    pub current: File,
    pub executor_stdout: File,
    pub executor_stderr: File,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestHistoryEntry {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<devcoordinator2_api::work_context::WorkAttribution>,
    pub run_id: String,
    pub test: String,
    pub status: TestStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<RunTerminationReason>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryCheckEvidence {
    pub name: String,
    pub status: devcoordinator2_executor_protocol::LeafStatus,
    pub duration_seconds: Option<f64>,
    pub exit: DiagnosticExit,
    pub artifacts: Vec<ArtifactReceipt>,
    pub streams: Vec<LogStreamSummary>,
    /// Added by schema-2 diagnostic receipts. Keep these optional so an
    /// upgraded daemon can recover a retained receipt written by a newer
    /// executor without rejecting the whole worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_diagnostics: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes_observed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_bytes_observed: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub case_selection: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<devcoordinator2_api::work_context::WorkAttribution>,
    pub run_id: String,
    pub test: String,
    pub proof: ProofKind,
    pub status: RunStatus,
    pub source_digest: String,
    pub config_digest: String,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub selection: Vec<String>,
    pub checks: Vec<RetryCheckEvidence>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryDocument {
    schema: Schema2,
    runs: Vec<TestHistoryEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceDocument {
    schema: Schema2,
    runs: Vec<RetryEvidence>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TestRunStore;

pub(crate) struct ContainerMutationLease {
    // Release exclusive use before the operation lock. A held exclusive-use
    // lease must never look like a fixture after its operation lock is released.
    _active_use: File,
    _operation: File,
}

pub(crate) enum ContainerLeaseAccess {
    Ready(ContainerMutationLease),
    ActiveUse,
    OperationInProgress,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ActiveTestArchiveBlocker;

impl crate::repository::ArchiveBlockerQuery for ActiveTestArchiveBlocker {
    fn first_blocker(
        &self,
        _repository_id: &str,
        repository_root: &Path,
    ) -> Result<Option<String>, devcoordinator2_api::ProtocolError> {
        match TestRunStore.read_current_summary(repository_root) {
            Ok(Some(summary)) if summary.status == TestStatus::Running => {
                Ok(Some("repository still has an active test".into()))
            }
            Ok(_) => Ok(None),
            Err(error) => Err(devcoordinator2_api::ProtocolError::new(
                devcoordinator2_api::ErrorCode::RepositoryArchiveBlocked,
                "cannot verify whether the repository has an active test",
            )
            .with_detail(error.to_string())),
        }
    }
}

impl TestRunStore {
    pub fn owner(&self, directory: &File) -> Result<(u32, u32), TestStateError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = directory
            .metadata()
            .map_err(|error| filesystem("inspect governed-test owner", error))?;
        Ok((metadata.uid(), metadata.gid()))
    }

    pub fn prepare(
        &self,
        worktree: &Path,
        run_id: &str,
        uid: u32,
        gid: u32,
    ) -> Result<PreparedRun, TestStateError> {
        validate_run_id(run_id)?;
        let root = open_worktree(worktree)?;
        let devcoordinator = ensure_directory(&root, ".devcoordinator", 0o755, Some((uid, gid)))?;
        let test = ensure_directory(&devcoordinator, "test", 0o755, Some((uid, gid)))?;
        remove_child_if_present(&test, "current")?;
        let current = create_directory(&test, "current", 0o755, Some((uid, gid)))?;
        create_directory(&current, "artifacts", 0o755, Some((uid, gid)))?;
        create_directory(&current, "scratch", 0o755, Some((uid, gid)))?;

        let logs = ensure_directory(&test, "logs", 0o711, None)?;
        let runs = ensure_directory(&logs, "runs", 0o711, None)?;
        let run = create_directory(&runs, run_id, 0o700, Some((uid, gid)))?;
        create_file(&run, "active.lock", 0o600, uid, gid, true)?;
        create_file(&run, "finalization.pending", 0o600, uid, gid, true)?;
        let executor = create_directory(&run, "executor", 0o700, Some((uid, gid)))?;
        let executor_stdout = create_file(&executor, "stdout.log", 0o600, uid, gid, true)?;
        let executor_stderr = create_file(&executor, "stderr.log", 0o600, uid, gid, true)?;
        current
            .sync_all()
            .map_err(|error| filesystem("sync current test directory", error))?;
        run.sync_all()
            .map_err(|error| filesystem("sync governed log run", error))?;
        Ok(PreparedRun {
            current_path: worktree.join(".devcoordinator/test/current"),
            log_path: worktree.join(".devcoordinator/test/logs/runs").join(run_id),
            current,
            executor_stdout,
            executor_stderr,
        })
    }

    pub fn prepare_log_metadata(
        &self,
        worktree: &Path,
        summary: &TestSummary,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        use devcoordinator2_executor_core::RunLogMetadata;
        let root = open_worktree(worktree)?;
        let run = open_chain(
            &root,
            &[".devcoordinator", "test", "logs", "runs", &summary.run_id],
        )?
        .ok_or_else(|| TestStateError::Invalid("log run directory is missing".into()))?;
        let started = time::PrimitiveDateTime::parse(
            &summary.started_at,
            &time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z"),
        )
        .map_err(|_| TestStateError::Invalid("run start timestamp is invalid".into()))?
        .assume_utc();
        let metadata = RunLogMetadata {
            schema: 2,
            run_id: summary.run_id.clone(),
            test: summary.test.clone(),
            started_at_epoch_ms: (started.unix_timestamp_nanos() / 1_000_000)
                .try_into()
                .map_err(|_| TestStateError::Invalid("negative run start".into()))?,
            finished_at_epoch_ms: None,
            status: RunStatus::Running,
            complete: false,
        };
        atomic_json(&run, "run.json", &metadata, 0o600, uid, gid)
    }

    pub fn finish_log_finalization(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<(), TestStateError> {
        validate_run_id(run_id)?;
        let root = open_worktree(worktree)?;
        if let Some(run) = open_chain(&root, &[".devcoordinator", "test", "logs", "runs", run_id])?
        {
            match unix_fs::unlinkat(&run, "finalization.pending", AtFlags::empty()) {
                Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                Err(error) => return Err(filesystem("remove finalization marker", error)),
            }
            run.sync_all()
                .map_err(|error| filesystem("sync log finalization", error))?;
        }
        Ok(())
    }

    pub fn remove_current(&self, worktree: &Path) -> Result<(), TestStateError> {
        let root = open_worktree(worktree)?;
        let Some(devcoordinator) = open_child(&root, ".devcoordinator")? else {
            return Ok(());
        };
        let Some(test) = open_child(&devcoordinator, "test")? else {
            return Ok(());
        };
        remove_child_if_present(&test, "current")
    }

    pub fn remove_log_run(&self, worktree: &Path, run_id: &str) -> Result<(), TestStateError> {
        validate_run_id(run_id)?;
        let root = open_worktree(worktree)?;
        let Some(test) = open_chain(&root, &[".devcoordinator", "test", "logs", "runs"])? else {
            return Ok(());
        };
        remove_child_if_present(&test, run_id)
    }

    pub fn write_summary(
        &self,
        current: &File,
        summary: &TestSummary,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        validate_summary(summary)?;
        atomic_json(current, SUMMARY_FILE, summary, 0o644, uid, gid)
    }

    pub fn read_current_summary(
        &self,
        worktree: &Path,
    ) -> Result<Option<TestSummary>, TestStateError> {
        let Some(root) = open_worktree_for_read(worktree)? else {
            return Ok(None);
        };
        let Some(current) = open_chain_for_read(&root, &[".devcoordinator", "test", "current"])?
        else {
            return Ok(None);
        };
        let summary = match read_json::<TestSummary>(&current, SUMMARY_FILE) {
            Ok(Some(summary)) => summary,
            Ok(None) | Err(TestStateError::Json(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if validate_summary(&summary).is_err() {
            return Ok(None);
        }
        Ok(Some(summary))
    }

    pub fn open_current(&self, worktree: &Path) -> Result<Option<File>, TestStateError> {
        let Some(root) = open_worktree_if_present(worktree)? else {
            return Ok(None);
        };
        open_chain(&root, &[".devcoordinator", "test", "current"])
    }

    pub fn write_plan(
        &self,
        current: &File,
        plan: &ExecutionPlan,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        plan.validate()
            .map_err(|error| TestStateError::Invalid(error.to_string()))?;
        atomic_json(current, PLAN_FILE, plan, 0o644, uid, gid)
    }

    pub fn read_report(&self, current: &File) -> Result<Option<ExecutionReport>, TestStateError> {
        let report = read_json::<ExecutionReport>(current, REPORT_FILE)?;
        if let Some(report) = &report {
            report
                .validate()
                .map_err(|_| TestStateError::Invalid("executor report failed validation".into()))?;
        }
        Ok(report)
    }

    pub fn write_environment(
        &self,
        current: &File,
        environment: &BTreeMap<String, String>,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        let mut payload = Vec::new();
        for (name, value) in environment {
            if !valid_environment_name(name) || value.contains(['\n', '\r']) {
                return Err(TestStateError::Invalid(
                    "test environment must contain safe single-line values".into(),
                ));
            }
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(payload, "{name}=\"{escaped}\"")
                .map_err(|error| TestStateError::Filesystem(error.to_string()))?;
        }
        atomic_bytes(current, ENV_FILE, &payload, 0o600, uid, gid)
    }

    pub fn write_database_environment(
        &self,
        current: &File,
        name: &str,
        environment: &BTreeMap<String, String>,
        uid: u32,
        gid: u32,
    ) -> Result<String, TestStateError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(TestStateError::Invalid(
                "invalid database environment identity".into(),
            ));
        }
        let file = format!("database-{name}.json");
        atomic_json(current, &file, environment, 0o600, uid, gid)?;
        Ok(file)
    }

    pub fn fixture_log(
        &self,
        current: &File,
        components: &[&str],
        uid: u32,
        gid: u32,
    ) -> Result<File, TestStateError> {
        let mut directory = current
            .try_clone()
            .map_err(|error| filesystem("open fixture log root", error))?;
        for component in components {
            directory = ensure_directory(&directory, component, 0o700, Some((uid, gid)))?;
        }
        create_file(&directory, "native.log", 0o600, uid, gid, true)
    }

    pub fn write_containers(
        &self,
        current: &File,
        containers: &[String],
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        atomic_json(current, CONTAINERS_FILE, containers, 0o600, uid, gid)
    }

    pub(crate) fn read_legacy_containers(
        &self,
        current: &File,
    ) -> Result<Vec<String>, TestStateError> {
        Ok(read_json::<Vec<String>>(current, CONTAINERS_FILE)?.unwrap_or_default())
    }

    pub(crate) fn seal_container_creation(
        &self,
        worktree: &Path,
        current: &File,
        run_id: &str,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        validate_run_id(run_id)?;
        // Serialize with intent publication when ownership already exists;
        // never allocate ownership metadata just to close an empty run.
        let _ownership = self.open_container_run(worktree, run_id, None)?;
        atomic_json(
            current,
            CONTAINER_CREATION_CLOSED_FILE,
            &run_id,
            0o600,
            uid,
            gid,
        )
    }

    pub(crate) fn container_creation_open(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<bool, TestStateError> {
        let Some(current) = self.open_current(worktree)? else {
            return Ok(false);
        };
        let Some(summary) = read_json::<TestSummary>(&current, SUMMARY_FILE)? else {
            return Ok(false);
        };
        validate_summary(&summary)?;
        Ok(summary.run_id == run_id
            && summary.status == TestStatus::Running
            && read_json::<String>(&current, CONTAINER_CREATION_CLOSED_FILE)?.is_none())
    }

    /// Retain daemon-owned temporary-container records separately from the
    /// legacy database-fixture ID list.  Keeping a typed record lets cleanup be
    /// retried after a daemon restart without trusting a caller-supplied ID.
    pub fn write_container_records(
        &self,
        current: &File,
        records: &[TestContainerRecord],
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        if records.len() > 256
            || records.iter().any(|record| {
                record.container_id.len() != 64
                    || !record
                        .container_id
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    || record.run_id.is_empty()
                    || record.check.is_empty()
                    || record.repository_id.is_empty()
                    || record.worktree_id.is_empty()
                    || record.owner_token.is_empty()
            })
        {
            return Err(TestStateError::Invalid(
                "temporary-container ownership records are invalid".into(),
            ));
        }
        atomic_json(current, CONTAINER_LIFECYCLE_FILE, records, 0o600, uid, gid)
    }

    pub fn read_container_records(
        &self,
        current: &File,
    ) -> Result<Vec<TestContainerRecord>, TestStateError> {
        let Some(records) =
            read_json::<Vec<TestContainerRecord>>(current, CONTAINER_LIFECYCLE_FILE)?
        else {
            return Ok(Vec::new());
        };
        if records.len() > 256
            || records.iter().any(|record| {
                record.container_id.len() != 64
                    || !record
                        .container_id
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    || record.run_id.is_empty()
                    || record.check.is_empty()
                    || record.repository_id.is_empty()
                    || record.worktree_id.is_empty()
                    || record.owner_token.is_empty()
            })
        {
            return Err(TestStateError::Invalid(
                "temporary-container ownership records are invalid".into(),
            ));
        }
        Ok(records)
    }

    /// Ownership outlives `current` replacement and log retention. The same
    /// typed receipt is stored once, in its run's private lifecycle directory.
    /// Lock the directory across each read/modify/atomic-write transaction.
    pub(crate) fn open_container_run(
        &self,
        worktree: &Path,
        run_id: &str,
        owner: Option<(u32, u32)>,
    ) -> Result<Option<File>, TestStateError> {
        validate_run_id(run_id)?;
        let Some(test) = self.open_test(worktree)? else {
            return Ok(None);
        };
        let mut directory = None;
        if let Some(containers) = open_child(&test, "containers")? {
            for state in ["pending", "receipts"] {
                if let Some(parent) = open_child(&containers, state)?
                    && let Some(found) = open_child(&parent, run_id)?
                {
                    directory = Some(found);
                    break;
                }
            }
        }
        if directory.is_none()
            && let Some(owner) = owner
        {
            let containers = ensure_directory(&test, "containers", 0o711, None)?;
            let pending = ensure_directory(&containers, "pending", 0o711, None)?;
            directory = Some(ensure_directory(&pending, run_id, 0o700, Some(owner))?);
        }
        if let Some(directory) = &directory {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
            loop {
                match unix_fs::flock(directory, FlockOperation::NonBlockingLockExclusive) {
                    Ok(()) => break,
                    Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(rustix::io::Errno::WOULDBLOCK) => return Err(TestStateError::Busy),
                    Err(error) => return Err(errno("lock container ownership", error)),
                }
            }
        }
        Ok(directory)
    }

    /// Retain a bounded container log inside the exact run ownership
    /// directory. The caller must already have authenticated the owner and
    /// hold the directory lease returned by `open_container_run`.
    pub(crate) fn write_container_diagnostics(
        &self,
        directory: &File,
        owner_token: &str,
        payload: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<String, TestStateError> {
        if payload.len() > CONTAINER_DIAGNOSTIC_CAP {
            return Err(TestStateError::Invalid(
                "container diagnostics exceed the bounded capture".into(),
            ));
        }
        validate_atom(owner_token)?;
        let diagnostics = ensure_directory(directory, "diagnostics", 0o700, Some((uid, gid)))?;
        let file = format!("{owner_token}.log");
        atomic_bytes(&diagnostics, &file, payload, 0o600, uid, gid)?;
        Ok(format!("diagnostics/{file}"))
    }

    pub(crate) fn read_container_diagnostics(
        &self,
        directory: &File,
        owner_token: &str,
    ) -> Result<Vec<u8>, TestStateError> {
        validate_atom(owner_token)?;
        let diagnostics = open_child(directory, "diagnostics")?.ok_or_else(|| {
            TestStateError::Invalid("retained container diagnostics are missing".into())
        })?;
        let file = File::from(
            unix_fs::openat(
                &diagnostics,
                format!("{owner_token}.log"),
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|error| errno("open retained container diagnostics", error))?,
        );
        let metadata = file
            .metadata()
            .map_err(|error| filesystem("inspect retained container diagnostics", error))?;
        if !metadata.is_file() || metadata.len() > CONTAINER_DIAGNOSTIC_CAP as u64 {
            return Err(TestStateError::Invalid(
                "retained container diagnostics are not a bounded regular file".into(),
            ));
        }
        let mut payload = Vec::new();
        file.take(CONTAINER_DIAGNOSTIC_CAP as u64 + 1)
            .read_to_end(&mut payload)
            .map_err(|error| filesystem("read retained container diagnostics", error))?;
        if payload.len() > CONTAINER_DIAGNOSTIC_CAP {
            return Err(TestStateError::Invalid(
                "retained container diagnostics exceed the capture bound".into(),
            ));
        }
        Ok(payload)
    }

    pub(crate) fn container_runs(&self, worktree: &Path) -> Result<Vec<String>, TestStateError> {
        let Some(test) = self.open_test(worktree)? else {
            return Ok(Vec::new());
        };
        let Some(containers) = open_chain(&test, &["containers", "pending"])? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        for entry in
            Dir::read_from(&containers).map_err(|error| errno("list container ownership", error))?
        {
            let entry = entry.map_err(|error| errno("read container ownership", error))?;
            let Some(name) = entry.file_name().to_str().ok() else {
                continue;
            };
            if validate_run_id(name).is_ok() {
                result.push(name.to_owned());
            }
        }
        result.sort();
        Ok(result)
    }

    pub(crate) fn container_run_pending(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<bool, TestStateError> {
        validate_run_id(run_id)?;
        let Some(test) = self.open_test(worktree)? else {
            return Ok(false);
        };
        Ok(open_chain(&test, &["containers", "pending", run_id])?.is_some())
    }

    pub(crate) fn archive_container_run(
        &self,
        worktree: &Path,
        run_id: &str,
        directory: &File,
    ) -> Result<(), TestStateError> {
        let records = self.read_container_records(directory)?;
        if records.iter().any(|record| {
            record.cleanup_status
                != devcoordinator2_api::test_containers::ContainerCleanupStatus::Complete
        }) {
            return Ok(());
        }
        let mut leases = Vec::new();
        for record in &records {
            let ContainerLeaseAccess::Ready(lease) =
                self.container_mutation_lease(directory, &record.owner_token)?
            else {
                return Ok(());
            };
            leases.push(lease);
        }
        let Some(test) = self.open_test(worktree)? else {
            return Ok(());
        };
        let Some(pending) = open_chain(&test, &["containers", "pending"])? else {
            return Ok(());
        };
        let Some(source) = open_child(&pending, run_id)? else {
            return Ok(());
        };
        use std::os::unix::fs::MetadataExt;
        if source
            .metadata()
            .map_err(|error| filesystem("inspect pending ownership", error))?
            .ino()
            != directory
                .metadata()
                .map_err(|error| filesystem("inspect owned receipt", error))?
                .ino()
        {
            return Err(TestStateError::Invalid(
                "container ownership directory changed".into(),
            ));
        }
        if records.is_empty()
            || read_json::<bool>(directory, "retention-expired.json")?.unwrap_or(false)
            || open_chain(&test, &["logs", "runs", run_id])?.is_none()
        {
            // Its ordinary run retention already expired while cleanup was
            // pending. The completed receipt now expires with that same run.
            self.validate_container_receipt_entries(directory, &records)?;
            return remove_child_if_present(&pending, run_id);
        }
        let containers = open_child(&test, "containers")?.ok_or_else(|| {
            TestStateError::Invalid("container ownership root disappeared".into())
        })?;
        let receipts = ensure_directory(&containers, "receipts", 0o711, None)?;
        if open_child(&receipts, run_id)?.is_some() {
            return Err(TestStateError::Invalid(
                "completed container receipt already exists".into(),
            ));
        }
        unix_fs::renameat(&pending, run_id, &receipts, run_id)
            .map_err(|error| errno("archive completed container receipt", error))?;
        pending
            .sync_all()
            .map_err(|error| filesystem("sync pending ownership", error))?;
        receipts
            .sync_all()
            .map_err(|error| filesystem("sync completed ownership", error))?;
        Ok(())
    }

    /// The existing run-retention receipt calls this exact-run cleanup. Live
    /// leases and any pending/failed record keep their ownership indefinitely.
    pub(crate) fn retire_container_run(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<(), TestStateError> {
        let Some(directory) = self.open_container_run(worktree, run_id, None)? else {
            return Ok(());
        };
        let records = self.read_container_records(&directory)?;
        let (uid, gid) = self.owner(&directory)?;
        atomic_json(&directory, "retention-expired.json", &true, 0o600, uid, gid)?;
        if self
            .read_current_summary(worktree)?
            .is_some_and(|summary| summary.run_id == run_id)
            && let Some(current) = self.open_current(worktree)?
        {
            self.retire_legacy_container_receipts(&current, &records, uid, gid)?;
        }
        if records.iter().any(|record| {
            record.cleanup_status
                != devcoordinator2_api::test_containers::ContainerCleanupStatus::Complete
        }) {
            return Ok(());
        }
        let mut leases = Vec::new();
        for record in &records {
            let ContainerLeaseAccess::Ready(lease) =
                self.container_mutation_lease(&directory, &record.owner_token)?
            else {
                return Ok(());
            };
            leases.push(lease);
        }
        self.validate_container_receipt_entries(&directory, &records)?;
        let Some(test) = self.open_test(worktree)? else {
            return Ok(());
        };
        for state in ["pending", "receipts"] {
            if let Some(parent) = open_chain(&test, &["containers", state])? {
                remove_child_if_present(&parent, run_id)?;
            }
        }
        Ok(())
    }

    fn validate_container_receipt_entries(
        &self,
        directory: &File,
        records: &[TestContainerRecord],
    ) -> Result<(), TestStateError> {
        for entry in
            Dir::read_from(directory).map_err(|error| errno("inspect container receipt", error))?
        {
            let entry = entry.map_err(|error| errno("read container receipt", error))?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| TestStateError::Invalid("container receipt name is invalid".into()))?;
            if name == "diagnostics" {
                let diagnostics = open_child(directory, name)?.ok_or_else(|| {
                    TestStateError::Invalid("container diagnostics directory vanished".into())
                })?;
                for entry in Dir::read_from(&diagnostics)
                    .map_err(|error| errno("inspect container diagnostic entries", error))?
                {
                    let entry =
                        entry.map_err(|error| errno("read container diagnostic entry", error))?;
                    let name = entry.file_name().to_str().map_err(|_| {
                        TestStateError::Invalid("container diagnostic name is invalid".into())
                    })?;
                    if matches!(name, "." | "..") {
                        continue;
                    }
                    let record = records
                        .iter()
                        .find(|record| {
                            record.diagnostic.is_some()
                                && name == format!("{}.log", record.owner_token)
                        })
                        .ok_or_else(|| {
                            TestStateError::Invalid(
                                "container diagnostics contain an unowned entry".into(),
                            )
                        })?;
                    self.read_container_diagnostics(directory, &record.owner_token)?;
                }
                continue;
            }
            if !matches!(
                name,
                "." | ".." | CONTAINER_LIFECYCLE_FILE | "retention-expired.json"
            ) && !records.iter().any(|record| {
                name == format!("{}.lease", record.owner_token)
                    || name == format!("{}.operation", record.owner_token)
            }) {
                return Err(TestStateError::Invalid(
                    "container receipt contains an unowned entry".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn retire_legacy_container_receipts(
        &self,
        current: &File,
        durable: &[TestContainerRecord],
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        let legacy = self.read_container_records(current)?;
        let retained = legacy
            .iter()
            .filter(|old| {
                !durable.iter().any(|record| {
                    record.owner_token == old.owner_token
                        && (record.container_id == old.container_id
                            || old.container_id == "0".repeat(64))
                        && record.run_id == old.run_id
                        && record.check == old.check
                        && record.repository_id == old.repository_id
                        && record.worktree_id == old.worktree_id
                        && record.name == old.name
                        && record.labels == old.labels
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if retained.len() != legacy.len() {
            self.write_container_records(current, &retained, uid, gid)?;
        }
        let legacy_ids = self.read_legacy_containers(current)?;
        let retained_ids = legacy_ids
            .iter()
            .filter(|id| {
                !durable.iter().any(|record| {
                    record.container_id == **id && record.container_id != "0".repeat(64)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if retained_ids.len() != legacy_ids.len() {
            self.write_containers(current, &retained_ids, uid, gid)?;
        }
        Ok(())
    }

    /// A live fixture or admitted creation holds a shared use lease. Mutation
    /// takes the separate operation lease first, then this use lease exclusively.
    pub(crate) fn container_lease(
        &self,
        directory: &File,
        token: &str,
        active: bool,
    ) -> Result<Option<File>, TestStateError> {
        validate_atom(token)?;
        let file = File::from(
            unix_fs::openat(
                directory,
                format!("{token}.lease"),
                OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| errno("open container lease", error))?,
        );
        let operation = if active {
            FlockOperation::NonBlockingLockShared
        } else {
            FlockOperation::NonBlockingLockExclusive
        };
        match unix_fs::flock(&file, operation) {
            Ok(()) => Ok(Some(file)),
            Err(rustix::io::Errno::WOULDBLOCK) if !active => Ok(None),
            Err(error) => Err(errno("lock container lease", error)),
        }
    }

    pub(crate) fn container_operation_lease(
        &self,
        directory: &File,
        token: &str,
    ) -> Result<Option<File>, TestStateError> {
        validate_atom(token)?;
        let file = File::from(
            unix_fs::openat(
                directory,
                format!("{token}.operation"),
                OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| errno("open container operation lease", error))?,
        );
        match unix_fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(file)),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(error) => Err(errno("lock container operation lease", error)),
        }
    }

    pub(crate) fn container_mutation_lease(
        &self,
        directory: &File,
        token: &str,
    ) -> Result<ContainerLeaseAccess, TestStateError> {
        let Some(operation) = self.container_operation_lease(directory, token)? else {
            return Ok(ContainerLeaseAccess::OperationInProgress);
        };
        let Some(active_use) = self.container_lease(directory, token, false)? else {
            return Ok(ContainerLeaseAccess::ActiveUse);
        };
        Ok(ContainerLeaseAccess::Ready(ContainerMutationLease {
            _active_use: active_use,
            _operation: operation,
        }))
    }

    pub fn read_history(&self, worktree: &Path) -> Result<Vec<TestHistoryEntry>, TestStateError> {
        let Some(test) = self.open_test(worktree)? else {
            return Ok(Vec::new());
        };
        let Some(document) = read_json::<HistoryDocument>(&test, HISTORY_FILE)? else {
            return Ok(Vec::new());
        };
        if document.runs.len() > HISTORY_CAP
            || document
                .runs
                .iter()
                .any(|run| !is_terminal(&run.status) || validate_run_id(&run.run_id).is_err())
        {
            return Err(TestStateError::Invalid(
                "test history contains invalid runs".into(),
            ));
        }
        Ok(document.runs)
    }

    pub fn record_history(
        &self,
        worktree: &Path,
        summary: &TestSummary,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        if !is_terminal(&summary.status) {
            return Ok(());
        }
        let test = self
            .open_test(worktree)?
            .ok_or_else(|| TestStateError::Invalid("test state directory is unavailable".into()))?;
        let mut runs = self.read_history(worktree)?;
        runs.retain(|run| run.run_id != summary.run_id);
        runs.push(TestHistoryEntry {
            targets: summary.targets.clone(),
            work: summary.work.clone(),
            run_id: summary.run_id.clone(),
            test: summary.test.clone(),
            status: summary.status.clone(),
            started_at: summary.started_at.clone(),
            finished_at: summary.finished_at.clone(),
            duration_seconds: summary.duration_seconds,
            exit_code: summary.exit_code,
            termination_reason: summary.termination_reason.clone(),
        });
        if runs.len() > HISTORY_CAP {
            runs.drain(..runs.len() - HISTORY_CAP);
        }
        bound_receipt_rows(&mut runs)?;
        atomic_json(
            &test,
            HISTORY_FILE,
            &HistoryDocument {
                schema: Schema2,
                runs,
            },
            0o644,
            uid,
            gid,
        )
    }

    pub fn read_evidence(&self, worktree: &Path) -> Result<Vec<RetryEvidence>, TestStateError> {
        let Some(test) = self.open_test(worktree)? else {
            return Ok(Vec::new());
        };
        let Some(document) = read_evidence_document(&test)? else {
            return Ok(Vec::new());
        };
        if document.runs.len() > EVIDENCE_CAP
            || document.runs.iter().any(|run| {
                validate_run_id(&run.run_id).is_err()
                    || run.status == RunStatus::Running
                    || run.readiness_eligible
                        != (run.proof == ProofKind::Complete
                            && run.requested_tier == ValidationTier::Release)
            })
        {
            return Err(TestStateError::Invalid(
                "test evidence contains invalid runs".into(),
            ));
        }
        Ok(document.runs)
    }

    pub fn find_evidence(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<Option<RetryEvidence>, TestStateError> {
        validate_run_id(run_id)?;
        let Some(test) = self.open_test(worktree)? else {
            return Ok(None);
        };
        let Some(value) = read_json::<serde_json::Value>(&test, EVIDENCE_FILE)? else {
            return Ok(None);
        };
        let schema = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TestStateError::Invalid("test evidence schema is missing".into()))?;
        if schema != 2 {
            return Err(TestStateError::Invalid(
                "test evidence schema is invalid".into(),
            ));
        }
        let Some(runs) = value.get("runs").and_then(serde_json::Value::as_array) else {
            return Err(TestStateError::Invalid(
                "test evidence runs must be an array".into(),
            ));
        };
        for value in runs.iter().rev() {
            let Some(candidate_id) = value.get("run_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if candidate_id != run_id {
                continue;
            }
            let mut candidate = value.clone();
            if let Some(checks) = candidate
                .get_mut("checks")
                .and_then(serde_json::Value::as_array_mut)
            {
                for check in checks {
                    if let Some(object) = check.as_object_mut()
                        && !object.contains_key("exit")
                        && let Some(exit_code) = object.remove("exit_code")
                    {
                        object.insert(
                            "exit".into(),
                            serde_json::json!({"code": exit_code, "signal": null}),
                        );
                    }
                }
            }
            let run: RetryEvidence = serde_json::from_value(candidate).map_err(|error| {
                TestStateError::Json(format!(
                    "retained input for run {run_id} is invalid: {error}"
                ))
            })?;
            if run.status == RunStatus::Running
                || run.readiness_eligible
                    != (run.proof == ProofKind::Complete
                        && run.requested_tier == ValidationTier::Release)
            {
                return Err(TestStateError::Invalid(format!(
                    "retained input for run {run_id} failed validation"
                )));
            }
            return Ok(Some(run));
        }
        Ok(None)
    }

    /// Resolve one history row without allowing an unrelated stale row to
    /// hide the exact run requested by a review or retry operation.
    pub fn find_history(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<Option<TestHistoryEntry>, TestStateError> {
        validate_run_id(run_id)?;
        let Some(test) = self.open_test(worktree)? else {
            return Ok(None);
        };
        let Some(value) = read_json::<serde_json::Value>(&test, HISTORY_FILE)? else {
            return Ok(None);
        };
        let schema = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TestStateError::Invalid("test history schema is missing".into()))?;
        if schema != 2 {
            return Err(TestStateError::Invalid(
                "test history schema is invalid".into(),
            ));
        }
        let Some(runs) = value.get("runs").and_then(serde_json::Value::as_array) else {
            return Err(TestStateError::Invalid(
                "test history runs must be an array".into(),
            ));
        };
        for value in runs.iter().rev() {
            if value.get("run_id").and_then(serde_json::Value::as_str) != Some(run_id) {
                continue;
            }
            let run: TestHistoryEntry = serde_json::from_value(value.clone()).map_err(|error| {
                TestStateError::Json(format!("history for run {run_id} is invalid: {error}"))
            })?;
            if !is_terminal(&run.status) {
                return Err(TestStateError::Invalid(format!(
                    "history for run {run_id} is not terminal"
                )));
            }
            return Ok(Some(run));
        }
        Ok(None)
    }

    pub fn record_evidence(
        &self,
        worktree: &Path,
        report: &ExecutionReport,
        terminal_status: &TestStatus,
        work: Option<&devcoordinator2_api::work_context::WorkAttribution>,
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        if *terminal_status == TestStatus::Running {
            return Err(TestStateError::Invalid(
                "cannot retain retry evidence for an active run".into(),
            ));
        }
        // The outer unit may end before its executor can publish a final report.
        // Retain a derived diagnostic receipt while preserving the original bytes.
        let interrupted = report.status == RunStatus::Running;
        let test = self
            .open_test(worktree)?
            .ok_or_else(|| TestStateError::Invalid("test state directory is unavailable".into()))?;
        let mut runs = self.read_evidence(worktree)?;
        runs.retain(|run| run.run_id != report.run_id);
        runs.push(RetryEvidence {
            targets: self
                .read_current_summary(worktree)?
                .filter(|summary| summary.run_id == report.run_id)
                .map(|summary| summary.targets)
                .unwrap_or_default(),
            case_selection: self
                .read_current_summary(worktree)?
                .filter(|summary| summary.run_id == report.run_id)
                .map(|summary| summary.case_selection)
                .unwrap_or_default(),
            work: work.cloned(),
            run_id: report.run_id.clone(),
            test: report.test.clone(),
            proof: report.proof,
            status: if *terminal_status == TestStatus::Passed && report.status == RunStatus::Passed
            {
                RunStatus::Passed
            } else {
                RunStatus::Failed
            },
            source_digest: report.source_digest.clone(),
            config_digest: report.config_digest.clone(),
            requested_tier: report.requested_tier,
            readiness_eligible: report.readiness_eligible,
            selection: report.selection.clone(),
            checks: report
                .checks
                .iter()
                .map(|check| {
                    let mut evidence = retry_check(check);
                    if interrupted
                        && matches!(
                            evidence.status,
                            devcoordinator2_executor_protocol::LeafStatus::Pending
                                | devcoordinator2_executor_protocol::LeafStatus::Running
                        )
                    {
                        evidence.status = if evidence.status
                            == devcoordinator2_executor_protocol::LeafStatus::Running
                            && *terminal_status == TestStatus::TimedOut
                        {
                            devcoordinator2_executor_protocol::LeafStatus::TimedOut
                        } else {
                            devcoordinator2_executor_protocol::LeafStatus::Cancelled
                        };
                    }
                    evidence
                })
                .collect(),
        });
        if runs.len() > EVIDENCE_CAP {
            runs.drain(..runs.len() - EVIDENCE_CAP);
        }
        bound_receipt_rows(&mut runs)?;
        atomic_json(
            &test,
            EVIDENCE_FILE,
            &EvidenceDocument {
                schema: Schema2,
                runs,
            },
            0o600,
            uid,
            gid,
        )
    }

    fn open_test(&self, worktree: &Path) -> Result<Option<File>, TestStateError> {
        let root = open_worktree(worktree)?;
        open_chain(&root, &[".devcoordinator", "test"])
    }
}

/// Decode retained retry evidence written by pre-schema-2 executors.
///
/// Older receipts represented a check's process result as an integer
/// `exit_code`; schema 2 stores the same value in the structured `exit`
/// object. Keep accepting that historical form so a terminal run remains
/// retryable after the daemon is upgraded, while still applying the strict
/// schema and unknown-field checks to every other field.
fn read_evidence_document(parent: &File) -> Result<Option<EvidenceDocument>, TestStateError> {
    match read_json::<EvidenceDocument>(parent, EVIDENCE_FILE) {
        Ok(document) => Ok(document),
        Err(TestStateError::Json(_)) => {
            let Some(mut value) = read_json::<serde_json::Value>(parent, EVIDENCE_FILE)? else {
                return Ok(None);
            };
            let Some(runs) = value
                .get_mut("runs")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return Err(TestStateError::Json(
                    "evidence runs must be an array".into(),
                ));
            };
            for run in runs {
                let Some(checks) = run
                    .get_mut("checks")
                    .and_then(serde_json::Value::as_array_mut)
                else {
                    continue;
                };
                for check in checks {
                    let Some(object) = check.as_object_mut() else {
                        continue;
                    };
                    if !object.contains_key("exit")
                        && let Some(exit_code) = object.remove("exit_code")
                    {
                        object.insert(
                            "exit".into(),
                            serde_json::json!({"code": exit_code, "signal": null}),
                        );
                    }
                }
            }
            serde_json::from_value(value)
                .map(Some)
                .map_err(|error| TestStateError::Json(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn initial_summary(
    run_id: &str,
    test: &str,
    started_at: &str,
    caller_uid: u32,
    client: &str,
    proof: ProofKind,
    selection: Vec<String>,
    origin_run_id: Option<String>,
    requested_tier: ValidationTier,
) -> TestSummary {
    TestSummary {
        targets: Vec::new(),
        case_selection: BTreeMap::new(),
        phase_durations: Vec::new(),
        work: None,
        schema_version: 2,
        run_id: run_id.into(),
        test: test.into(),
        status: TestStatus::Running,
        started_at: started_at.into(),
        finished_at: None,
        duration_seconds: None,
        exit_code: None,
        stdout_bytes_observed: 0,
        stderr_bytes_observed: 0,
        failed_diagnostics: None,
        native_execution_started: None,
        container_cleanup_status: None,
        container_lifecycle_ref: None,
        caller_uid,
        execution_uid: None,
        client: client.into(),
        proof: api_proof(proof),
        selection,
        origin_run_id,
        requested_tier: api_tier(requested_tier),
        readiness_eligible: proof == ProofKind::Complete
            && requested_tier == ValidationTier::Release,
        check_report_ref: REPORT_FILE.into(),
        report_issue: None,
        log_catalog_ref: LogCatalogReference {
            run_id: run_id.into(),
        },
        termination_reason: None,
        check_summary: None,
        checks: None,
        checks_truncated: None,
        failure_index: None,
        failure_index_truncated: None,
        source_changed: None,
        execution_capacity: None,
        capacity_wait_count: None,
        capacity: None,
    }
}

pub fn terminal_status(
    summary: &mut TestSummary,
    status: TestStatus,
    finished_at: String,
    duration_seconds: f64,
    exit_code: Option<i32>,
    termination_reason: Option<RunTerminationReason>,
) {
    summary.status = status;
    summary.finished_at = Some(finished_at);
    summary.duration_seconds = Some(duration_seconds.max(0.0));
    summary.exit_code = exit_code;
    summary.termination_reason = termination_reason;
}

fn retry_check(check: &CheckReport) -> RetryCheckEvidence {
    RetryCheckEvidence {
        name: check.name.clone(),
        status: check.status,
        duration_seconds: check.duration_seconds,
        exit: check.exit,
        artifacts: check.artifacts.clone(),
        streams: check.streams.clone(),
        failed_diagnostics: Some(check.failed_diagnostics),
        stdout_bytes_observed: None,
        stderr_bytes_observed: None,
    }
}

fn validate_summary(summary: &TestSummary) -> Result<(), TestStateError> {
    if summary.execution_uid == Some(0) {
        return Err(TestStateError::Invalid(
            "repository execution UID must not be root".into(),
        ));
    }
    validate_run_id(&summary.run_id)?;
    let memory_stop = summary.termination_reason == Some(RunTerminationReason::MemoryPressure);
    if summary.schema_version != 2
        || summary.test.is_empty()
        || summary.test.len() > 32
        || summary.check_report_ref != REPORT_FILE
        || summary.log_catalog_ref.run_id != summary.run_id
        || summary.readiness_eligible
            != (summary.proof == ApiProofKind::Complete
                && summary.requested_tier == devcoordinator2_api::params::ValidationTier::Release
                && summary.report_issue.is_none()
                && !memory_stop)
        || (memory_stop && summary.status != TestStatus::Failed)
        || (summary.status == TestStatus::Passed
            && matches!(
                summary.container_cleanup_status,
                Some(
                    devcoordinator2_api::test_containers::ContainerCleanupStatus::Pending
                        | devcoordinator2_api::test_containers::ContainerCleanupStatus::Failed
                )
            ))
    {
        return Err(TestStateError::Invalid("test summary is invalid".into()));
    }
    match summary.proof {
        ApiProofKind::Complete
            if !summary.selection.is_empty() || summary.origin_run_id.is_some() =>
        {
            Err(TestStateError::Invalid(
                "complete test summary has selection".into(),
            ))
        }
        ApiProofKind::Selected
            if summary.selection.is_empty() || summary.origin_run_id.is_some() =>
        {
            Err(TestStateError::Invalid(
                "selected test summary is invalid".into(),
            ))
        }
        ApiProofKind::Retry
            if summary.selection.len() != 1
                || summary.origin_run_id.as_deref().is_none_or(str::is_empty) =>
        {
            Err(TestStateError::Invalid(
                "retry test summary is invalid".into(),
            ))
        }
        _ => Ok(()),
    }
}

fn is_terminal(status: &TestStatus) -> bool {
    !matches!(status, TestStatus::Running)
}

fn api_proof(proof: ProofKind) -> ApiProofKind {
    match proof {
        ProofKind::Complete => ApiProofKind::Complete,
        ProofKind::Selected => ApiProofKind::Selected,
        ProofKind::Retry => ApiProofKind::Retry,
    }
}

pub fn api_tier(tier: ValidationTier) -> devcoordinator2_api::params::ValidationTier {
    match tier {
        ValidationTier::Development => devcoordinator2_api::params::ValidationTier::Development,
        ValidationTier::PreMerge => devcoordinator2_api::params::ValidationTier::PreMerge,
        ValidationTier::Release => devcoordinator2_api::params::ValidationTier::Release,
    }
}

pub fn executor_tier(tier: devcoordinator2_api::params::ValidationTier) -> ValidationTier {
    match tier {
        devcoordinator2_api::params::ValidationTier::Development => ValidationTier::Development,
        devcoordinator2_api::params::ValidationTier::PreMerge => ValidationTier::PreMerge,
        devcoordinator2_api::params::ValidationTier::Release => ValidationTier::Release,
    }
}

fn open_worktree(path: &Path) -> Result<File, TestStateError> {
    open_worktree_if_present(path)?
        .ok_or_else(|| errno("open worktree root", rustix::io::Errno::NOENT))
}

fn open_worktree_if_present(path: &Path) -> Result<Option<File>, TestStateError> {
    open_worktree_path(path, false)
}

fn open_worktree_for_read(path: &Path) -> Result<Option<File>, TestStateError> {
    open_worktree_path(path, true)
}

fn open_worktree_path(
    path: &Path,
    tolerate_non_directory: bool,
) -> Result<Option<File>, TestStateError> {
    if !path.is_absolute() {
        return Err(TestStateError::Invalid(
            "worktree root must be absolute".into(),
        ));
    }
    match unix_fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        // A retained worktree can disappear between registration and a list
        // read. Treat a path whose parent was replaced by a file, or whose
        // symlink target was retired, as absent so one stale checkout cannot
        // hide every other test run. Mutating operations pass `false` and
        // retain the concrete error.
        Err(error)
            if tolerate_non_directory
                && matches!(error, rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) =>
        {
            Ok(None)
        }
        Err(error) => Err(errno("open worktree root", error)),
    }
}

fn open_chain(root: &File, names: &[&str]) -> Result<Option<File>, TestStateError> {
    let mut directory = root
        .try_clone()
        .map_err(|error| filesystem("duplicate directory descriptor", error))?;
    for name in names {
        let Some(child) = open_child(&directory, name)? else {
            return Ok(None);
        };
        directory = child;
    }
    Ok(Some(directory))
}

/// Read-only summary lookups must tolerate a retained checkout being replaced
/// by a file or symlink at any path component. The registration may outlive
/// that checkout, and one malformed entry must not make a global test listing
/// fail for every other worktree. Mutating paths continue to use `open_chain`
/// so they retain the concrete filesystem error.
fn open_chain_for_read(root: &File, names: &[&str]) -> Result<Option<File>, TestStateError> {
    let mut directory = root
        .try_clone()
        .map_err(|error| filesystem("duplicate directory descriptor", error))?;
    for name in names {
        let Some(child) = open_child_for_read(&directory, name)? else {
            return Ok(None);
        };
        directory = child;
    }
    Ok(Some(directory))
}

fn open_child(parent: &File, name: &str) -> Result<Option<File>, TestStateError> {
    validate_atom(name)?;
    match unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(errno("open governed-test directory", error)),
    }
}

fn open_child_for_read(parent: &File, name: &str) -> Result<Option<File>, TestStateError> {
    validate_atom(name)?;
    match unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(rustix::io::Errno::NOENT)
        | Err(rustix::io::Errno::NOTDIR)
        | Err(rustix::io::Errno::LOOP) => Ok(None),
        Err(error) => Err(errno("open governed-test directory", error)),
    }
}

fn ensure_directory(
    parent: &File,
    name: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<File, TestStateError> {
    validate_atom(name)?;
    match unix_fs::mkdirat(parent, name, Mode::from_raw_mode(mode as _)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(errno("create governed-test directory", error)),
    }
    let directory = open_child(parent, name)?
        .ok_or_else(|| TestStateError::Filesystem("governed-test directory vanished".into()))?;
    unix_fs::fchmod(&directory, Mode::from_raw_mode(mode as _))
        .map_err(|error| errno("set governed-test directory mode", error))?;
    if let Some((uid, gid)) = owner {
        fchown(&directory, uid, gid)?;
    }
    Ok(directory)
}

fn create_directory(
    parent: &File,
    name: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<File, TestStateError> {
    validate_atom(name)?;
    unix_fs::mkdirat(parent, name, Mode::from_raw_mode(mode as _)).map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            TestStateError::Invalid(format!("governed-test directory {name:?} already exists"))
        } else {
            errno("create governed-test directory", error)
        }
    })?;
    ensure_directory(parent, name, mode, owner)
}

fn create_file(
    parent: &File,
    name: &str,
    mode: u32,
    uid: u32,
    gid: u32,
    exclusive: bool,
) -> Result<File, TestStateError> {
    validate_atom(name)?;
    let mut flags = OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if exclusive {
        flags |= OFlags::EXCL;
    } else {
        flags |= OFlags::TRUNC;
    }
    let file = unix_fs::openat(parent, name, flags, Mode::from_raw_mode(mode as _))
        .map(File::from)
        .map_err(|error| errno("create governed-test file", error))?;
    unix_fs::fchmod(&file, Mode::from_raw_mode(mode as _))
        .map_err(|error| errno("set governed-test file mode", error))?;
    fchown(&file, uid, gid)?;
    Ok(file)
}

fn bound_receipt_rows<T: Serialize>(rows: &mut Vec<T>) -> Result<(), TestStateError> {
    let sizes = rows
        .iter()
        .map(|row| serde_json::to_vec(row).map(|bytes| bytes.len() + 1))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TestStateError::Json(error.to_string()))?;
    let mut total = sizes.iter().sum::<usize>() + 64;
    let mut discard = 0;
    for size in sizes.into_iter().take(rows.len().saturating_sub(1)) {
        if total <= JSON_LIMIT as usize {
            break;
        }
        total -= size;
        discard += 1;
    }
    if total > JSON_LIMIT as usize {
        return Err(TestStateError::Invalid(
            "newest governed-run receipt exceeds the 2 MiB limit".into(),
        ));
    }
    rows.drain(..discard);
    Ok(())
}

#[cfg(test)]
#[path = "work_context_state_tests.rs"]
mod work_context_tests;

fn atomic_json<T: Serialize + ?Sized>(
    parent: &File,
    name: &str,
    value: &T,
    mode: u32,
    uid: u32,
    gid: u32,
) -> Result<(), TestStateError> {
    let mut payload =
        serde_json::to_vec(value).map_err(|error| TestStateError::Json(error.to_string()))?;
    payload.push(b'\n');
    atomic_bytes(parent, name, &payload, mode, uid, gid)
}

fn atomic_bytes(
    parent: &File,
    name: &str,
    payload: &[u8],
    mode: u32,
    uid: u32,
    gid: u32,
) -> Result<(), TestStateError> {
    if payload.len() as u64 > JSON_LIMIT {
        return Err(TestStateError::Invalid(
            "governed-test file exceeds the 2 MiB limit".into(),
        ));
    }
    validate_atom(name)?;
    let temporary = format!(
        ".{name}.{}.tmp",
        crate::ids::bug_id().map_err(|error| { TestStateError::Filesystem(error.to_string()) })?
    );
    let result = (|| {
        let mut file = create_file(parent, &temporary, mode, uid, gid, true)?;
        file.write_all(payload)
            .map_err(|error| filesystem("write governed-test file", error))?;
        file.sync_all()
            .map_err(|error| filesystem("sync governed-test file", error))?;
        unix_fs::renameat(parent, temporary.as_str(), parent, name)
            .map_err(|error| errno("publish governed-test file", error))?;
        parent
            .sync_all()
            .map_err(|error| filesystem("sync governed-test directory", error))
    })();
    if result.is_err() {
        let _ = unix_fs::unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn read_json<T: DeserializeOwned>(parent: &File, name: &str) -> Result<Option<T>, TestStateError> {
    validate_atom(name)?;
    let descriptor = match unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(errno("open governed-test JSON", error)),
    };
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| filesystem("inspect governed-test JSON", error))?;
    if !metadata.is_file() || metadata.len() > JSON_LIMIT {
        return Err(TestStateError::Invalid(
            "governed-test JSON is not a bounded regular file".into(),
        ));
    }
    let mut payload = Vec::new();
    file.take(JSON_LIMIT + 1)
        .read_to_end(&mut payload)
        .map_err(|error| filesystem("read governed-test JSON", error))?;
    if payload.len() as u64 > JSON_LIMIT {
        return Err(TestStateError::Invalid(
            "governed-test JSON exceeds the 2 MiB limit".into(),
        ));
    }
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| TestStateError::Json(error.to_string()))
}

fn remove_child_if_present(parent: &File, name: &str) -> Result<(), TestStateError> {
    validate_atom(name)?;
    let Some(directory) = open_child(parent, name)? else {
        match unix_fs::unlinkat(parent, name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(errno("remove governed-test entry", error)),
        }
    };
    remove_contents(&directory)?;
    unix_fs::unlinkat(parent, name, AtFlags::REMOVEDIR)
        .map_err(|error| errno("remove governed-test directory", error))?;
    parent
        .sync_all()
        .map_err(|error| filesystem("sync removed governed-test directory", error))
}

fn remove_contents(directory: &File) -> Result<(), TestStateError> {
    let mut names = Vec::new();
    let mut entries =
        Dir::read_from(directory).map_err(|error| errno("list governed-test directory", error))?;
    for entry in &mut entries {
        let entry = entry.map_err(|error| errno("read governed-test directory", error))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(
                CString::new(name).map_err(|_| {
                    TestStateError::Invalid("governed-test entry contains NUL".into())
                })?,
            );
        }
    }
    for name in names {
        match unix_fs::openat(
            directory,
            name.as_c_str(),
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let child = File::from(descriptor);
                remove_contents(&child)?;
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::REMOVEDIR)
                    .map_err(|error| errno("remove governed-test subdirectory", error))?;
            }
            Err(_) => {
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::empty())
                    .map_err(|error| errno("remove governed-test file", error))?;
            }
        }
    }
    Ok(())
}

fn validate_run_id(value: &str) -> Result<(), TestStateError> {
    if !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && b"._-".contains(&byte))
        })
    {
        Ok(())
    } else {
        Err(TestStateError::Invalid(
            "invalid governed-test run identifier".into(),
        ))
    }
}

fn validate_atom(value: &str) -> Result<(), TestStateError> {
    if !value.is_empty()
        && value.len() <= 255
        && !value.as_bytes().contains(&b'/')
        && value != "."
        && value != ".."
    {
        Ok(())
    } else {
        Err(TestStateError::Invalid(
            "governed-test path component is invalid".into(),
        ))
    }
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn fchown(file: &File, uid: u32, gid: u32) -> Result<(), TestStateError> {
    // SAFETY: `file` owns a live descriptor and fchown does not retain it.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } == 0 {
        Ok(())
    } else {
        Err(filesystem(
            "set governed-test owner",
            io::Error::last_os_error(),
        ))
    }
}

fn filesystem(operation: &str, error: impl std::fmt::Display) -> TestStateError {
    TestStateError::Filesystem(format!("{operation}: {error}"))
}

fn errno(operation: &str, error: rustix::io::Errno) -> TestStateError {
    filesystem(operation, io::Error::from(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    #[test]
    fn missing_or_non_directory_worktree_is_absent_from_read_only_state() {
        let temporary = tempdir().unwrap();
        let file_root = temporary.path().join("replaced-worktree");
        std::fs::write(&file_root, "stale scratch entry").unwrap();
        assert_eq!(TestRunStore.read_current_summary(&file_root).unwrap(), None);
        assert!(
            open_worktree(&file_root)
                .unwrap_err()
                .to_string()
                .contains("Not a directory")
        );

        let linked_root = temporary.path().join("linked-worktree");
        symlink(temporary.path().join("missing-target"), &linked_root).unwrap();
        assert_eq!(
            TestRunStore.read_current_summary(&linked_root).unwrap(),
            None
        );
        assert!(open_worktree(&linked_root).is_err());

        let missing_root = temporary.path().join("removed-worktree");
        assert_eq!(
            TestRunStore.read_current_summary(&missing_root).unwrap(),
            None
        );

        let nested_file_root = temporary.path().join("nested-file-worktree");
        std::fs::create_dir(&nested_file_root).unwrap();
        std::fs::write(
            nested_file_root.join(".devcoordinator"),
            "stale scratch entry",
        )
        .unwrap();
        assert_eq!(
            TestRunStore
                .read_current_summary(&nested_file_root)
                .unwrap(),
            None
        );

        let nested_link_root = temporary.path().join("nested-link-worktree");
        std::fs::create_dir(&nested_link_root).unwrap();
        symlink(
            temporary.path().join("missing-target"),
            nested_link_root.join(".devcoordinator"),
        )
        .unwrap();
        assert_eq!(
            TestRunStore
                .read_current_summary(&nested_link_root)
                .unwrap(),
            None
        );
    }

    #[test]
    fn run_layout_summary_plan_history_and_evidence_round_trip() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let store = TestRunStore;
        let prepared = store
            .prepare(&root, "t20260904T000000Z-aabbcc", uid, gid)
            .unwrap();
        let summary = initial_summary(
            "t20260904T000000Z-aabbcc",
            "all",
            "2026-09-04T00:00:00Z",
            uid,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        store
            .write_summary(&prepared.current, &summary, uid, gid)
            .unwrap();
        assert_eq!(
            store.read_current_summary(&root).unwrap(),
            Some(summary.clone())
        );
        assert_eq!(
            crate::repository::ArchiveBlockerQuery::first_blocker(
                &ActiveTestArchiveBlocker,
                "r1",
                &root,
            )
            .unwrap()
            .as_deref(),
            Some("repository still has an active test")
        );
        assert_eq!(
            std::fs::metadata(prepared.log_path.join("executor/stdout.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let mut terminal = summary;
        terminal_status(
            &mut terminal,
            TestStatus::Passed,
            "2026-09-04T00:00:01Z".into(),
            1.0,
            Some(0),
            None,
        );
        store.record_history(&root, &terminal, uid, gid).unwrap();
        store.record_history(&root, &terminal, uid, gid).unwrap();
        assert_eq!(store.read_history(&root).unwrap().len(), 1);
    }

    #[test]
    fn legacy_retry_evidence_exit_code_remains_readable() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let store = TestRunStore;
        store
            .prepare(&root, "t20260904T000000Z-aabbcc", uid, gid)
            .unwrap();
        let payload = serde_json::json!({
            "schema": 2,
            "runs": [{
                "run_id": "t20260904T000000Z-aabbcc",
                "test": "all",
                "proof": "complete",
                "status": "failed",
                "source_digest": "a".repeat(64),
                "config_digest": "b".repeat(64),
                "requested_tier": "release",
                "readiness_eligible": true,
                "selection": [],
                "checks": [{
                    "name": "unit",
                    "status": "failed",
                    "duration_seconds": 1.0,
                    "exit_code": 17,
                    "stderr_bytes_observed": 23,
                    "artifacts": [],
                    "streams": []
                }]
            }]
        });
        std::fs::write(
            root.join(".devcoordinator/test/evidence.json"),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        let evidence = store.read_evidence(&root).unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].checks[0].exit.code, Some(17));
        assert_eq!(evidence[0].checks[0].exit.signal, None);
        assert_eq!(evidence[0].checks[0].stderr_bytes_observed, Some(23));
    }

    #[test]
    fn replacing_current_and_cleanup_never_follow_repository_symlinks() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repository");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("preserved"), "yes").unwrap();
        std::fs::create_dir(root.join(".devcoordinator")).unwrap();
        symlink(&outside, root.join(".devcoordinator/test")).unwrap();
        let store = TestRunStore;
        assert!(
            store
                .prepare(
                    &root.canonicalize().unwrap(),
                    "t20260904T000000Z-aabbcc",
                    rustix::process::getuid().as_raw(),
                    rustix::process::getgid().as_raw(),
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("preserved")).unwrap(),
            "yes"
        );
    }

    #[test]
    fn summary_validation_rejects_cross_run_log_catalog_identity() {
        let mut summary = initial_summary(
            "t20260904T000000Z-aabbcc",
            "all",
            "2026-09-04T00:00:00Z",
            1,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        summary.log_catalog_ref.run_id = "other".into();
        assert!(validate_summary(&summary).is_err());
    }

    #[test]
    fn memory_emergency_summary_must_be_failed_and_ineligible() {
        let mut summary = initial_summary(
            "t20260904T000000Z-aabbcc",
            "all",
            "2026-09-04T00:00:00Z",
            1,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        assert!(validate_summary(&summary).is_ok());
        summary.termination_reason = Some(RunTerminationReason::MemoryPressure);
        summary.status = TestStatus::Failed;
        assert!(validate_summary(&summary).is_err());
        summary.readiness_eligible = false;
        assert!(validate_summary(&summary).is_ok());
        summary.status = TestStatus::Passed;
        assert!(validate_summary(&summary).is_err());
        summary.status = TestStatus::Running;
        assert!(validate_summary(&summary).is_err());
    }
}
