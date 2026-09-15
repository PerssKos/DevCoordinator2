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
use devcoordinator2_executor_protocol::{
    ArtifactReceipt, CheckReport, DiagnosticExit, ExecutionPlan, ExecutionReport, LogStreamSummary,
    ProofKind, RunStatus, Schema2, ValidationTier,
};
use rustix::fs::{self as unix_fs, AtFlags, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PLAN_FILE: &str = "check-plan.json";
pub const REPORT_FILE: &str = "check-report.json";
pub const SUMMARY_FILE: &str = "summary.json";
pub const ENV_FILE: &str = "env";
pub const CONTAINERS_FILE: &str = "containers.json";
const HISTORY_FILE: &str = "history.json";
const EVIDENCE_FILE: &str = "evidence.json";
const JSON_LIMIT: u64 = 2 * 1024 * 1024;
const HISTORY_CAP: usize = 1_000;
const EVIDENCE_CAP: usize = 50;

#[derive(Debug, Error)]
pub enum TestStateError {
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
        let Some(root) = open_worktree_if_present(worktree)? else {
            return Ok(None);
        };
        let Some(current) = open_chain(&root, &[".devcoordinator", "test", "current"])? else {
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

    pub fn write_containers(
        &self,
        current: &File,
        containers: &[String],
        uid: u32,
        gid: u32,
    ) -> Result<(), TestStateError> {
        atomic_json(current, CONTAINERS_FILE, containers, 0o600, uid, gid)
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
        let Some(document) = read_json::<EvidenceDocument>(&test, EVIDENCE_FILE)? else {
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
        Ok(self
            .read_evidence(worktree)?
            .into_iter()
            .rev()
            .find(|run| run.run_id == run_id))
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
        caller_uid,
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
    }
}

fn validate_summary(summary: &TestSummary) -> Result<(), TestStateError> {
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
