use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::progress::ProcessUpdate;
use devcoordinator2_executor_protocol::ExecutionProgress;
use devcoordinator2_executor_protocol::{
    ArtifactReceipt, CapacityReport, CaseManifest, CaseReport, CaseSpec, CheckPlan, CheckReport,
    CompletionMode, DiagnosticExit, DiagnosticOrigin, DiagnosticReportSource, ErrorCategory,
    ExecutionPlan, ExecutionReport, FailureIndexEntry, FailureMode, LeafStatus, LogPhase, LogRef,
    LogStream, LogStreamSummary, MAX_DIAGNOSTIC_EVENTS, MAX_MANIFEST_BYTES, MAX_REASON_BYTES,
    PhaseDuration, ResourceClaim, RetainedArtifactReceipt, RunStatus, Schema2, TerminationReason,
};
use rustix::process::Signal;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::ExecutorError;
use crate::capacity::{CapacityObservation, PermitProvider};
use crate::diagnostics::{
    DiagnosticContext, diagnostic_fingerprint, normalize_diagnostics, parse_declared_report,
};
use crate::evidence::{
    RetainedArtifactIdentity, artifact_receipts, receipts_match, retain_artifact_trees,
    source_digest, write_bytes_atomic, write_json_atomic,
};
use crate::process::{
    Cancellation, EventService, ProcessRequest, ProcessResult, ProcessStatus, ServiceExit,
    run_process, signal_group,
};
use crate::{LeafLogMetadata, LeafSelector, RunLogLease, RunLogMetadata, StreamMetadata};

const SERVICE_TERMINATION_GRACE: Duration = Duration::from_secs(2);
const MAX_INLINE_CASES: usize = 128;
const MAX_CASE_EVIDENCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DECLARED_DIAGNOSTIC_REPORT_BYTES: u64 = 16 * 1024 * 1024;

pub struct Executor {
    plan: ExecutionPlan,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    observations: watch::Sender<Option<ExecutionReport>>,
}

struct RunMetadataGuard {
    lease: Arc<RunLogLease>,
    run_id: String,
    test: String,
    started_at_epoch_ms: u64,
    finalized: bool,
}

impl RunMetadataGuard {
    fn start(
        lease: Arc<RunLogLease>,
        run_id: &str,
        test: &str,
        started_at_epoch_ms: u64,
    ) -> Result<Self, ExecutorError> {
        let guard = Self {
            lease,
            run_id: run_id.to_owned(),
            test: test.to_owned(),
            started_at_epoch_ms,
            finalized: false,
        };
        if let Err(error) = guard.lease.publish_run_metadata(&RunLogMetadata {
            schema: 2,
            run_id: run_id.to_owned(),
            test: test.to_owned(),
            started_at_epoch_ms,
            finished_at_epoch_ms: None,
            status: RunStatus::Running,
            complete: false,
        }) {
            let error = ExecutorError::new(error.to_string());
            drop(guard);
            return Err(error);
        }
        Ok(guard)
    }

    fn finish(&mut self, status: RunStatus, complete: bool) -> Result<(), ExecutorError> {
        self.lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: self.run_id.clone(),
                test: self.test.clone(),
                started_at_epoch_ms: self.started_at_epoch_ms,
                finished_at_epoch_ms: Some(epoch_ms()),
                status,
                complete,
            })
            .map_err(|error| ExecutorError::new(error.to_string()))?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for RunMetadataGuard {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let _ = self.lease.publish_run_metadata(&RunLogMetadata {
            schema: 2,
            run_id: self.run_id.clone(),
            test: self.test.clone(),
            started_at_epoch_ms: self.started_at_epoch_ms,
            finished_at_epoch_ms: Some(epoch_ms()),
            status: RunStatus::Failed,
            complete: false,
        });
    }
}

impl Executor {
    pub fn new(
        plan: ExecutionPlan,
        permits: Arc<dyn PermitProvider>,
        cancellation: Cancellation,
    ) -> Result<Self, ExecutorError> {
        plan.validate()?;
        Ok(Self {
            plan,
            permits,
            cancellation,
            observations: watch::channel(None).0,
        })
    }

    /// Observe reports after their atomic publication without polling the filesystem.
    pub fn subscribe_progress(&self) -> watch::Receiver<Option<ExecutionReport>> {
        self.observations.subscribe()
    }

    pub async fn run(self) -> Result<ExecutionReport, ExecutorError> {
        let plan = self.plan;
        let root_requested = PathBuf::from(&plan.worktree_root);
        let current_requested = PathBuf::from(&plan.current_dir);
        let log_requested = PathBuf::from(&plan.log_dir);
        if !current_requested.starts_with(&root_requested) {
            return Err(ExecutorError::new(
                "executor current_dir is outside the requested worktree",
            ));
        }
        let root = root_requested
            .canonicalize()
            .map_err(|error| ExecutorError::new(format!("cannot resolve worktree: {error}")))?;
        if !log_requested.starts_with(&root_requested) {
            return Err(ExecutorError::new(
                "executor log_dir is outside the requested worktree",
            ));
        }
        let log_dir = Arc::new(log_requested.canonicalize().map_err(|error| {
            ExecutorError::new(format!("cannot resolve pre-created log directory: {error}"))
        })?);
        if !log_dir.starts_with(&root) {
            return Err(ExecutorError::new(
                "executor log_dir resolves outside the worktree",
            ));
        }
        let log_lease = Arc::new(
            RunLogLease::acquire(&log_dir, &plan.run_id)
                .map_err(|error| ExecutorError::new(error.to_string()))?,
        );
        let started_at = iso_now();
        let started_epoch_ms = epoch_ms();
        let started = Instant::now();
        let mut run_metadata = RunMetadataGuard::start(
            log_lease.clone(),
            &plan.run_id,
            &plan.test,
            started_epoch_ms,
        )?;
        let current = current_requested.canonicalize().map_err(|error| {
            ExecutorError::new(format!("cannot resolve pre-created run directory: {error}"))
        })?;
        if !current.starts_with(&root) {
            return Err(ExecutorError::new(
                "executor current_dir resolves outside the worktree",
            ));
        }
        let plan = Arc::new(plan);
        for directory in ["checks", "scratch", "artifacts"] {
            tokio::fs::create_dir_all(current.join(directory))
                .await
                .map_err(|error| {
                    ExecutorError::new(format!("cannot create run artifacts: {error}"))
                })?;
        }
        let digest_root = root.clone();
        let initial_digest = tokio::task::spawn_blocking(move || source_digest(&digest_root))
            .await
            .map_err(|error| ExecutorError::new(format!("source digest task failed: {error}")))??;
        if initial_digest != plan.source_digest {
            return Err(ExecutorError::new(
                "repository source changed before checks started",
            ));
        }

        let capacity = Arc::new(Mutex::new(CapacityReport::default()));
        let mut checks = initialize_checks(&plan, &root, &started_at)?;
        publish_reused_leaf_metadata(&mut checks, &plan.run_id, &log_lease, started_epoch_ms);
        let report_path = current.join("check-report.json");
        write_report(
            &report_path,
            &self.observations,
            &plan,
            &checks,
            &capacity,
            &started_at,
            started,
            RunStatus::Running,
            None,
            false,
            None,
        )?;

        let invalidators = invalidator_map(&checks);
        let mut running = JoinSet::<(String, CheckOutcome)>::new();
        let mut services = JoinSet::<(String, ServiceExit)>::new();
        let mut service_pgids = BTreeMap::<String, i32>::new();
        let mut abort: Option<Abort> = None;
        let mut cancellation_seen = false;
        let (progress, mut progress_rx) = unbounded_channel();

        loop {
            if !service_pgids.is_empty()
                && checks.iter().all(|check| {
                    check.plan.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup
                        || check.report.status.is_terminal()
                })
            {
                stop_services(
                    &mut services,
                    &mut service_pgids,
                    &mut checks,
                    &plan,
                    &log_lease,
                    &log_dir,
                )
                .await?;
            }
            while let Ok(update) = progress_rx.try_recv() {
                apply_process_update(&mut checks, &capacity, update)?;
            }
            for check in &mut checks {
                refresh_execution(check);
            }
            while let Some(joined) = services.try_join_next() {
                let (name, exit) = joined.map_err(|error| {
                    ExecutorError::new(format!("event service task failed: {error}"))
                })?;
                service_pgids.remove(&name);
                mark_service_exit(&mut checks, &plan, &log_lease, &log_dir, &name, exit, false)?;
                if abort.is_none() {
                    abort = Some(Abort::Unsafe(format!(
                        "required long-lived check {name} exited"
                    )));
                    self.cancellation.cancel();
                }
            }
            mark_blocked(&mut checks, &invalidators, &plan.run_id, &log_lease);
            if self.cancellation.is_cancelled() && abort.is_none() {
                abort = Some(Abort::Cancelled);
            }
            if abort.is_some() {
                cancellation_seen = true;
                mark_pending_cancelled(
                    &mut checks,
                    &plan.run_id,
                    &log_lease,
                    abort_reason(abort.as_ref()),
                    !matches!(abort, Some(Abort::Unsafe(_))),
                );
            }

            if !matches!(abort, Some(Abort::Unsafe(_))) {
                let ready = ready_checks_with_available_resources(
                    &checks,
                    &invalidators,
                    service_pgids.keys().map(String::as_str),
                );
                for name in ready {
                    let index = check_index(&checks, &name)?;
                    if abort.is_some()
                        && checks[index].plan.phase
                            != devcoordinator2_executor_protocol::CheckPhase::Cleanup
                    {
                        continue;
                    }
                    let runtime = &mut checks[index];
                    runtime.report.status = LeafStatus::Running;
                    runtime.report.started_at = None;
                    runtime.report.resource_waiting = !runtime.plan.resources.is_empty();
                    runtime.started_epoch_ms = Some(epoch_ms());
                    let check = runtime.plan.clone();
                    let consumed_expected = check
                        .consumes
                        .iter()
                        .map(|consumed| {
                            checks
                                .iter()
                                .find(|producer| producer.plan.name == consumed.check)
                                .and_then(|producer| {
                                    producer
                                        .report
                                        .artifacts
                                        .iter()
                                        .find(|receipt| receipt.path == consumed.path)
                                })
                                .cloned()
                                .ok_or_else(|| {
                                    ExecutorError::new(
                                        "consumed artifact has no completed producer receipt",
                                    )
                                })
                        })
                        .collect::<Result<Vec<_>, _>>();
                    let plan = plan.clone();
                    let root = root.clone();
                    let current = current.clone();
                    let permits = self.permits.clone();
                    let cancellation =
                        if check.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup {
                            Cancellation::default()
                        } else {
                            self.cancellation.clone()
                        };
                    let log_lease = log_lease.clone();
                    let log_dir = log_dir.clone();
                    let progress = progress.clone();
                    running.spawn(async move {
                        let outcome = execute_check(
                            consumed_expected,
                            progress.clone(),
                            plan,
                            check,
                            root,
                            current,
                            permits,
                            cancellation,
                            log_lease,
                            log_dir,
                        )
                        .await;
                        (name, outcome)
                    });
                }
            }

            write_report(
                &report_path,
                &self.observations,
                &plan,
                &checks,
                &capacity,
                &started_at,
                started,
                RunStatus::Running,
                None,
                false,
                None,
            )?;

            if checks.iter().all(|check| check.report.status.is_terminal()) {
                break;
            }
            if running.is_empty() {
                return Err(ExecutorError::new("executor graph made no progress"));
            }

            tokio::select! {
                Some(update) = progress_rx.recv() => { apply_process_update(&mut checks, &capacity, update)?; }
                joined = running.join_next(), if !running.is_empty() => {
                    let Some(joined) = joined else { continue };
                    let (name, outcome) = joined.map_err(|error| {
                        ExecutorError::new(format!("check task failed: {error}"))
                    })?;
                    let index = check_index(&checks, &name)?;
                    let failure_mode = checks[index].plan.on_failure;
                    let status = outcome.status;
                    apply_outcome(&mut checks[index], outcome, &mut services, &mut service_pgids);
                    if abort.is_none() && (status == LeafStatus::Unsafe
                        || (!status.is_success() && failure_mode == FailureMode::Stop))
                    {
                        abort = Some(if status == LeafStatus::Unsafe {
                            Abort::Unsafe(format!("check {name} became unsafe"))
                        } else {
                            Abort::Stopped(format!("check {name} requested stop on failure"))
                        });
                        self.cancellation.cancel();
                    }
                }
                joined = services.join_next(), if !services.is_empty() => {
                    let Some(joined) = joined else { continue };
                    let (name, exit) = joined.map_err(|error| {
                        ExecutorError::new(format!("event service task failed: {error}"))
                    })?;
                    service_pgids.remove(&name);
                    mark_service_exit(
                        &mut checks,
                        &plan,
                        &log_lease,
                        &log_dir,
                        &name,
                        exit,
                        false,
                    )?;
                    if abort.is_none() {
                        abort = Some(Abort::Unsafe(format!(
                            "required long-lived check {name} exited"
                        )));
                        self.cancellation.cancel();
                    }
                }
                () = self.cancellation.cancelled(), if !cancellation_seen => {
                    cancellation_seen = true;
                    if abort.is_none() {
                        abort = Some(Abort::Cancelled);
                    }
                }
            }
        }

        stop_services(
            &mut services,
            &mut service_pgids,
            &mut checks,
            &plan,
            &log_lease,
            &log_dir,
        )
        .await?;
        while let Ok(update) = progress_rx.try_recv() {
            apply_process_update(&mut checks, &capacity, update)?;
        }
        for check in &mut checks {
            refresh_execution(check);
        }
        let digest_root = root.clone();
        let final_digest = tokio::task::spawn_blocking(move || source_digest(&digest_root)).await;
        let (source_changed, source_error) = match final_digest {
            Ok(Ok(digest)) => (digest != plan.source_digest, None),
            Ok(Err(error)) => (true, Some(format!("cannot verify final source: {error}"))),
            Err(error) => (
                true,
                Some(format!("final source digest task failed: {error}")),
            ),
        };
        let status = if source_changed
            || abort.is_some()
            || checks.iter().any(|check| !check.report.status.is_success())
        {
            RunStatus::Failed
        } else {
            RunStatus::Passed
        };
        let finished_at = iso_now();
        let mut report = write_report(
            &report_path,
            &self.observations,
            &plan,
            &checks,
            &capacity,
            &started_at,
            started,
            status,
            Some(finished_at),
            source_changed,
            source_error,
        )?;
        let sealed = run_metadata.finish(status, true).is_ok();
        if !sealed {
            report.status = RunStatus::Failed;
            report.failure_index.push(failure_entry(
                &plan.run_id,
                None,
                None,
                LeafStatus::Unsafe,
                None,
                Some(TerminationReason::UnsafeStop),
                ErrorCategory::LogStorage,
                None,
            ));
            let normalized = normalize_diagnostics(report.failure_index)
                .map_err(|_| ExecutorError::new("cannot normalize run log storage failure"))?;
            report.failure_index = normalized.entries;
            report.failure_index_truncated |= normalized.truncated;
            write_json_atomic(&report_path, &report)?;
            publish_observation(&self.observations, &report);
        }
        if sealed && !source_changed {
            for check in &checks {
                if check.report.status != LeafStatus::Passed {
                    continue;
                }
                let Some(key) = check.cache_key.as_deref() else {
                    continue;
                };
                let Ok(inputs) = artifact_receipts(&root, &check.plan.cache_inputs) else {
                    continue;
                };
                let Ok(consumed) = artifact_receipts(
                    &root,
                    &check
                        .plan
                        .consumes
                        .iter()
                        .map(|artifact| artifact.path.clone())
                        .collect::<Vec<_>>(),
                ) else {
                    continue;
                };
                if inputs == check.report.cache_inputs
                    && consumed == check.report.consumed_artifacts
                    && crate::reuse::key(&root, &plan, &check.plan, &inputs, &consumed).as_deref()
                        == Some(key)
                    && receipts_match(&root, &check.report.artifacts).unwrap_or(false)
                {
                    crate::reuse::record(&root, &plan, &check.plan, key, &check.report.artifacts);
                }
            }
        }
        Ok(report)
    }
}

fn apply_process_update(
    checks: &mut [CheckRuntime],
    capacity: &Mutex<CapacityReport>,
    update: ProcessUpdate,
) -> Result<(), ExecutorError> {
    if let Some(observation) = update.capacity {
        observe_capacity(capacity, observation);
    } else if update.progress.finished == 1 && update.progress.admitted_at_epoch_ms.is_none() {
        observe_capacity(
            capacity,
            CapacityObservation {
                waited: true,
                ..Default::default()
            },
        );
    }
    let name = update
        .selector
        .check
        .as_deref()
        .ok_or_else(|| ExecutorError::new("process update has no check"))?;
    let index = check_index(checks, name)?;
    checks[index]
        .processes
        .insert(update.leaf_id.clone(), update);
    refresh_execution(&mut checks[index]);
    Ok(())
}

fn refresh_execution(check: &mut CheckRuntime) {
    if !check.processes.is_empty() {
        check.report.resource_waiting = false;
    }
    if check.processes.is_empty() {
        return;
    }
    let mut execution = ExecutionProgress {
        queued_at_epoch_ms: u64::MAX,
        ..Default::default()
    };
    for update in check.processes.values() {
        let p = &update.progress;
        execution.waiting += p.waiting;
        execution.admitted += p.admitted;
        execution.executing += p.executing;
        execution.finished += p.finished;
        execution.queued_at_epoch_ms = execution.queued_at_epoch_ms.min(p.queued_at_epoch_ms);
        execution.admitted_at_epoch_ms = execution
            .admitted_at_epoch_ms
            .into_iter()
            .chain(p.admitted_at_epoch_ms)
            .min();
        execution.started_at_epoch_ms = execution
            .started_at_epoch_ms
            .into_iter()
            .chain(p.started_at_epoch_ms)
            .min();
        execution.finished_at_epoch_ms = execution
            .finished_at_epoch_ms
            .into_iter()
            .chain(p.finished_at_epoch_ms)
            .max();
        execution.capacity_wait_ms = execution
            .capacity_wait_ms
            .saturating_add(p.capacity_wait_ms);
        execution.process_duration_ms = execution
            .process_duration_ms
            .saturating_add(p.process_duration_ms);
        if let Some(case_id) = &update.selector.case_id {
            if let Some(case) = check
                .report
                .cases
                .iter_mut()
                .find(|case| &case.id == case_id)
            {
                case.execution = Some(p.clone());
                if let Some(status) = update.status
                    && case.phases.is_empty()
                {
                    case.status = status;
                }
                if case.phases.is_empty() {
                    case.duration_ms = p.process_duration_ms;
                }
            } else if !check.report.status.is_terminal() && check.report.cases.len() < 64 {
                check.report.cases.push(CaseReport {
                    phases: Vec::new(),
                    execution: Some(p.clone()),
                    id: case_id.clone(),
                    status: update.status.unwrap_or(LeafStatus::Running),
                    exit: DiagnosticExit::default(),
                    duration_ms: 0,
                    streams: Vec::new(),
                });
            }
        }
    }
    for case in &mut check.report.cases {
        let members = check
            .processes
            .values()
            .filter(|update| update.selector.case_id.as_deref() == Some(case.id.as_str()))
            .collect::<Vec<_>>();
        let native = members
            .iter()
            .any(|update| matches!(update.selector.phase, LogPhase::Fixture | LogPhase::Cleanup));
        if !native {
            continue;
        }
        let mut combined = ExecutionProgress {
            queued_at_epoch_ms: u64::MAX,
            ..Default::default()
        };
        for update in &members {
            let p = &update.progress;
            combined.waiting += p.waiting;
            combined.admitted += p.admitted;
            combined.executing += p.executing;
            combined.finished += p.finished;
            combined.queued_at_epoch_ms = combined.queued_at_epoch_ms.min(p.queued_at_epoch_ms);
            combined.admitted_at_epoch_ms = combined
                .admitted_at_epoch_ms
                .into_iter()
                .chain(p.admitted_at_epoch_ms)
                .min();
            combined.started_at_epoch_ms = combined
                .started_at_epoch_ms
                .into_iter()
                .chain(p.started_at_epoch_ms)
                .min();
            combined.finished_at_epoch_ms = combined
                .finished_at_epoch_ms
                .into_iter()
                .chain(p.finished_at_epoch_ms)
                .max();
            combined.capacity_wait_ms =
                combined.capacity_wait_ms.saturating_add(p.capacity_wait_ms);
            combined.process_duration_ms = combined
                .process_duration_ms
                .saturating_add(p.process_duration_ms);
            let phase = match update.selector.phase {
                LogPhase::Fixture => devcoordinator2_executor_protocol::CheckPhase::Fixture,
                LogPhase::Cleanup => devcoordinator2_executor_protocol::CheckPhase::Cleanup,
                _ => devcoordinator2_executor_protocol::CheckPhase::Case,
            };
            if let Some(observed) = case.phases.iter_mut().find(|entry| entry.phase == phase) {
                observed.duration_ms = p.process_duration_ms;
            }
        }
        if combined.waiting + combined.admitted + combined.executing > 0 {
            combined.finished_at_epoch_ms = None;
        }
        if case.phases.is_empty() {
            case.status = if members.iter().any(|update| {
                update.selector.phase == LogPhase::Cleanup && update.progress.finished == 1
            }) {
                members
                    .iter()
                    .filter_map(|update| update.status)
                    .find(|status| !status.is_success())
                    .unwrap_or(LeafStatus::Passed)
            } else {
                LeafStatus::Running
            };
        }
        case.duration_ms = combined.process_duration_ms;
        case.execution = Some(combined);
    }
    if execution.waiting + execution.admitted + execution.executing > 0 {
        execution.finished_at_epoch_ms = None;
    }
    if let Some(started) = execution.started_at_epoch_ms {
        check.report.started_at = Some(iso_at(started / 1000));
    }
    if !check.report.status.is_terminal() {
        check.report.case_count = check
            .processes
            .values()
            .filter_map(|update| update.selector.case_id.as_ref())
            .collect::<BTreeSet<_>>()
            .len()
            .try_into()
            .unwrap_or(u32::MAX);
        check.report.cases_truncated = check.report.case_count as usize > check.report.cases.len();
    }
    if check.report.status.is_terminal() {
        let first = check
            .processes
            .values()
            .filter_map(|update| update.started_mono)
            .min();
        let last = if check.plan.completion == CompletionMode::Event {
            check.completed_mono
        } else {
            check
                .processes
                .values()
                .filter_map(|update| update.finished_mono)
                .max()
        };
        if let (Some(first), Some(last)) = (first, last) {
            check.report.duration_seconds = Some(seconds(last.saturating_duration_since(first)));
        }
    }
    check.report.execution = Some(execution);
}

struct CheckRuntime {
    cache_key: Option<String>,
    completed_mono: Option<Instant>,
    processes: BTreeMap<String, ProcessUpdate>,
    plan: CheckPlan,
    report: CheckReport,
    failures: Vec<FailureIndexEntry>,
    started_epoch_ms: Option<u64>,
}

struct CheckOutcome {
    cache_key: Option<String>,
    status: LeafStatus,
    exit_code: Option<i32>,
    reason: Option<String>,
    artifacts: Vec<ArtifactReceipt>,
    cache_inputs: Vec<ArtifactReceipt>,
    consumed_artifacts: Vec<ArtifactReceipt>,
    retained_artifacts: Vec<RetainedArtifactReceipt>,
    streams: Vec<LogStreamSummary>,
    case_count: u32,
    cases: Vec<CaseReport>,
    cases_truncated: bool,
    failures: Vec<FailureIndexEntry>,
    service: Option<EventService>,
    duration_seconds: f64,
}

#[derive(Clone)]
struct CaseOutcome {
    report: CaseReport,
    failures: Vec<FailureIndexEntry>,
}

enum Abort {
    Cancelled,
    Stopped(String),
    Unsafe(String),
}

#[allow(clippy::too_many_arguments)]
fn failure_entry(
    run_id: &str,
    check: Option<&str>,
    case: Option<&str>,
    status: LeafStatus,
    exit_code: Option<i32>,
    termination_reason: Option<TerminationReason>,
    error_category: ErrorCategory,
    phase: Option<LogPhase>,
) -> FailureIndexEntry {
    let exit = match exit_code {
        Some(value) if value < 0 => DiagnosticExit {
            code: None,
            signal: u8::try_from(value.unsigned_abs()).ok(),
        },
        code => DiagnosticExit { code, signal: None },
    };
    let log_refs = match (check, phase) {
        (Some(check), Some(phase)) => [LogStream::Stdout, LogStream::Stderr]
            .into_iter()
            .map(|stream| LogRef {
                run_id: run_id.to_owned(),
                check: Some(check.to_owned()),
                phase,
                case: case.map(str::to_owned),
                stream,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut entry = FailureIndexEntry {
        check: check.map(str::to_owned),
        case: case.map(str::to_owned),
        status,
        exit,
        termination_reason,
        source: None,
        error_category,
        expected: None,
        actual: None,
        fingerprint: String::new(),
        occurrences: 1,
        log_refs,
        origin: DiagnosticOrigin::Executor,
    };
    entry.fingerprint = diagnostic_fingerprint(&entry);
    entry
}

fn process_failure_semantics(
    process: &ProcessResult,
    completion: CompletionMode,
) -> (Option<TerminationReason>, ErrorCategory) {
    if process.log_storage_failed {
        return (
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::LogStorage,
        );
    }
    if process.structured_evidence_invalid {
        return (
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::StructuredEvidenceInvalid,
        );
    }
    if completion == CompletionMode::Event
        && process.status == ProcessStatus::Failed
        && !process.reason.as_deref().is_some_and(|reason| {
            reason.contains("before its completion event")
                || reason.contains("event process exited")
        })
    {
        return (None, ErrorCategory::Exception);
    }
    leaf_failure_semantics(
        process.status.into(),
        process.exit_code,
        process.reason.as_deref(),
    )
}

fn leaf_failure_semantics(
    status: LeafStatus,
    exit_code: Option<i32>,
    reason: Option<&str>,
) -> (Option<TerminationReason>, ErrorCategory) {
    match status {
        LeafStatus::Failed if exit_code.is_some() => (None, ErrorCategory::ProcessExit),
        LeafStatus::Failed => (None, ErrorCategory::Internal),
        LeafStatus::TimedOut => (
            Some(TerminationReason::DeadlineExceeded),
            ErrorCategory::Timeout,
        ),
        LeafStatus::Cancelled => (
            Some(TerminationReason::RunCancelled),
            ErrorCategory::Cancellation,
        ),
        LeafStatus::Unsafe
            if reason.is_some_and(|reason| {
                reason.contains("leaf output")
                    || reason.contains("retain leaf output")
                    || reason.contains("sync leaf output")
                    || reason.contains("log storage")
                    || reason.contains("leaf log metadata")
                    || reason.contains("stdout task")
                    || reason.contains("stderr task")
            }) =>
        {
            (
                Some(TerminationReason::UnsafeStop),
                ErrorCategory::LogStorage,
            )
        }
        LeafStatus::Unsafe if reason.is_some_and(|reason| reason.contains("event")) => (
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::StructuredEvidenceInvalid,
        ),
        LeafStatus::Unsafe => (Some(TerminationReason::UnsafeStop), ErrorCategory::Internal),
        LeafStatus::Invalidated | LeafStatus::NotMeaningful => (None, ErrorCategory::Dependency),
        LeafStatus::Pending | LeafStatus::Running | LeafStatus::Reused | LeafStatus::Passed => {
            (None, ErrorCategory::Internal)
        }
    }
}

fn initialize_checks(
    plan: &ExecutionPlan,
    root: &Path,
    started_at: &str,
) -> Result<Vec<CheckRuntime>, ExecutorError> {
    let mut result = Vec::new();
    for check in plan.active_checks() {
        let cwd = root.join(&check.cwd);
        let resolved = cwd.canonicalize().map_err(|error| {
            ExecutorError::new(format!("cannot resolve cwd for {:?}: {error}", check.name))
        })?;
        if !resolved.starts_with(root) {
            return Err(ExecutorError::new(format!(
                "check {:?} cwd escapes the worktree",
                check.name
            )));
        }
        let reused = plan.reused.get(&check.name);
        let reused_qualification = plan.reused_qualifications.contains(&check.name);
        if let Some(receipts) = reused
            && !receipts_match(root, receipts)?
        {
            return Err(ExecutorError::new(format!(
                "reused evidence for {:?} is stale",
                check.name
            )));
        }
        let status = if reused.is_some() || reused_qualification {
            LeafStatus::Reused
        } else {
            LeafStatus::Pending
        };
        result.push(CheckRuntime {
            cache_key: None,
            plan: check.clone(),
            completed_mono: None,
            processes: BTreeMap::new(),
            report: CheckReport {
                phase_durations: Vec::new(),
                resource_waiting: false,
                display_name: check.display_name.clone(),
                execution: None,
                name: check.name.clone(),
                tier: check.tier,
                role: check.role,
                phase: check.phase,
                fingerprint: check.fingerprint.clone(),
                cache_inputs: artifact_receipts(root, &check.cache_inputs)?,
                consumed_artifacts: Vec::new(),
                status,
                started_at: (reused.is_some() || reused_qualification)
                    .then(|| started_at.to_owned()),
                finished_at: (reused.is_some() || reused_qualification)
                    .then(|| started_at.to_owned()),
                duration_seconds: (reused.is_some() || reused_qualification).then_some(0.0),
                exit: DiagnosticExit::default(),
                artifacts: reused.cloned().unwrap_or_default(),
                retained_artifacts: Vec::new(),
                streams: Vec::new(),
                case_count: 0,
                cases: Vec::new(),
                cases_truncated: false,
            },
            failures: Vec::new(),
            started_epoch_ms: (reused.is_some() || reused_qualification).then(epoch_ms),
        });
    }
    Ok(result)
}

fn invalidator_map(checks: &[CheckRuntime]) -> BTreeMap<String, Vec<String>> {
    let active: BTreeSet<&str> = checks
        .iter()
        .map(|check| check.plan.name.as_str())
        .collect();
    let mut result = BTreeMap::<String, Vec<String>>::new();
    for check in checks {
        for target in &check.plan.invalidates {
            if active.contains(target.as_str()) {
                result
                    .entry(target.clone())
                    .or_default()
                    .push(check.plan.name.clone());
            }
        }
    }
    for values in result.values_mut() {
        values.sort();
    }
    result
}

fn publish_reused_leaf_metadata(
    checks: &mut [CheckRuntime],
    run_id: &str,
    log_lease: &RunLogLease,
    started_epoch_ms: u64,
) {
    for check in checks {
        if check.report.status != LeafStatus::Reused {
            continue;
        }
        let selector = LeafSelector::check(check.plan.name.clone()).expect("validated selector");
        if log_lease
            .publish_leaf_metadata(&LeafLogMetadata {
                schema: 2,
                selector,
                status: LeafStatus::Reused,
                exit: DiagnosticExit::default(),
                started_at_epoch_ms: started_epoch_ms,
                finished_at_epoch_ms: Some(started_epoch_ms),
                process_started: false,
                complete: true,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .is_err()
        {
            check.report.status = LeafStatus::Unsafe;
            check.failures.push(failure_entry(
                run_id,
                Some(&check.plan.name),
                None,
                LeafStatus::Unsafe,
                None,
                Some(TerminationReason::UnsafeStop),
                ErrorCategory::LogStorage,
                Some(LogPhase::Check),
            ));
        }
    }
}

fn mark_blocked(
    checks: &mut [CheckRuntime],
    invalidators: &BTreeMap<String, Vec<String>>,
    run_id: &str,
    log_lease: &RunLogLease,
) {
    loop {
        let pending = checks
            .iter()
            .filter(|check| check.report.status == LeafStatus::Pending)
            .count();
        mark_blocked_once(checks, invalidators, run_id, log_lease);
        if checks
            .iter()
            .filter(|check| check.report.status == LeafStatus::Pending)
            .count()
            == pending
        {
            break;
        }
    }
}

fn mark_blocked_once(
    checks: &mut [CheckRuntime],
    invalidators: &BTreeMap<String, Vec<String>>,
    run_id: &str,
    log_lease: &RunLogLease,
) {
    let statuses: BTreeMap<String, LeafStatus> = checks
        .iter()
        .map(|check| (check.plan.name.clone(), check.report.status))
        .collect();
    for check in checks {
        if check.report.status != LeafStatus::Pending
            || check.plan.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup
        {
            continue;
        }
        if let Some(preflights) = invalidators.get(&check.plan.name) {
            let failed: Vec<&str> = preflights
                .iter()
                .filter(|name| {
                    statuses[name.as_str()].is_terminal() && !statuses[name.as_str()].is_success()
                })
                .map(String::as_str)
                .collect();
            if !failed.is_empty() {
                finish_blocked(
                    check,
                    run_id,
                    log_lease,
                    LeafStatus::Invalidated,
                    format!(
                        "invalidating preflights did not pass: {}",
                        failed.join(", ")
                    ),
                );
                continue;
            }
        }
        let dependencies: Vec<&str> = check
            .plan
            .requires
            .iter()
            .filter(|name| {
                statuses[name.as_str()].is_terminal() && !statuses[name.as_str()].is_success()
            })
            .map(String::as_str)
            .collect();
        let all_terminal = check
            .plan
            .requires
            .iter()
            .all(|name| statuses[name.as_str()].is_terminal());
        if all_terminal && !dependencies.is_empty() {
            finish_blocked(
                check,
                run_id,
                log_lease,
                LeafStatus::NotMeaningful,
                format!("required checks did not pass: {}", dependencies.join(", ")),
            );
        }
    }
}

fn finish_blocked(
    check: &mut CheckRuntime,
    run_id: &str,
    log_lease: &RunLogLease,
    status: LeafStatus,
    _reason: String,
) {
    check.report.status = status;
    check.report.finished_at = Some(iso_now());
    check.report.duration_seconds = Some(0.0);
    let (termination_reason, error_category) = match status {
        LeafStatus::Invalidated | LeafStatus::NotMeaningful => (None, ErrorCategory::Dependency),
        LeafStatus::Cancelled => (
            Some(TerminationReason::RunCancelled),
            ErrorCategory::Cancellation,
        ),
        LeafStatus::TimedOut => (
            Some(TerminationReason::DeadlineExceeded),
            ErrorCategory::Timeout,
        ),
        LeafStatus::Unsafe => (Some(TerminationReason::UnsafeStop), ErrorCategory::Internal),
        _ => (None, ErrorCategory::Internal),
    };
    let selector = LeafSelector::check(check.plan.name.clone()).expect("validated selector");
    let mut error_category = error_category;
    let mut termination_reason = termination_reason;
    if log_lease
        .publish_leaf_metadata(&LeafLogMetadata {
            schema: 2,
            selector,
            status,
            exit: DiagnosticExit::default(),
            started_at_epoch_ms: check.started_epoch_ms.unwrap_or_else(epoch_ms),
            finished_at_epoch_ms: Some(epoch_ms()),
            process_started: false,
            complete: true,
            structured_evidence_formats: Vec::new(),
            structured_evidence_count: 0,
        })
        .is_err()
    {
        check.report.status = LeafStatus::Unsafe;
        error_category = ErrorCategory::LogStorage;
        termination_reason = Some(TerminationReason::UnsafeStop);
    }
    check.failures.push(failure_entry(
        run_id,
        Some(&check.plan.name),
        None,
        check.report.status,
        None,
        termination_reason,
        error_category,
        None,
    ));
}

fn ready_checks(
    checks: &[CheckRuntime],
    invalidators: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let statuses: BTreeMap<&str, LeafStatus> = checks
        .iter()
        .map(|check| (check.plan.name.as_str(), check.report.status))
        .collect();
    checks
        .iter()
        .filter(|check| check.report.status == LeafStatus::Pending)
        .filter(|check| {
            check
                .plan
                .after
                .iter()
                .chain(&check.plan.requires)
                .all(|name| statuses[name.as_str()].is_terminal())
        })
        .filter(|check| {
            check.plan.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup
                || check
                    .plan
                    .requires
                    .iter()
                    .all(|name| statuses[name.as_str()].is_success())
        })
        .filter(|check| {
            check.plan.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup
                || invalidators
                    .get(&check.plan.name)
                    .into_iter()
                    .flatten()
                    .all(|name| statuses[name.as_str()].is_success())
        })
        .map(|check| check.plan.name.clone())
        .collect()
}

fn ready_checks_with_available_resources<'a>(
    checks: &[CheckRuntime],
    invalidators: &BTreeMap<String, Vec<String>>,
    live_services: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let services = live_services.collect::<BTreeSet<_>>();
    let mut claimed = checks
        .iter()
        .filter(|check| {
            check.report.status == LeafStatus::Running
                || services.contains(check.plan.name.as_str())
        })
        .flat_map(|check| check.plan.resources.iter().cloned())
        .collect::<Vec<_>>();
    let mut admitted = Vec::new();
    for name in ready_checks(checks, invalidators) {
        let Some(check) = checks.iter().find(|check| check.plan.name == name) else {
            continue;
        };
        if check.plan.resources.iter().any(|candidate| {
            claimed
                .iter()
                .any(|active| resources_conflict(candidate, active))
        }) {
            continue;
        }
        claimed.extend(check.plan.resources.iter().cloned());
        admitted.push(name);
    }
    admitted
}

fn resources_conflict(left: &ResourceClaim, right: &ResourceClaim) -> bool {
    left.conflicts(right)
}

fn read_database_environment(
    current: &Path,
    path: &str,
) -> Result<BTreeMap<String, String>, ExecutorError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
    use std::io::Read;
    if !path.starts_with("database-")
        || !path.ends_with(".json")
        || Path::new(path).components().count() != 1
    {
        return Err(ExecutorError::new(
            "invalid private database environment identity",
        ));
    }
    let directory = open(
        current,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| ExecutorError::new("private environment directory is unavailable"))?;
    let descriptor = openat(
        &directory,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| ExecutorError::new("private database environment is unavailable"))?;
    let stat = fstat(&descriptor)
        .map_err(|_| ExecutorError::new("private database environment cannot be inspected"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_mode & 0o077 != 0
        || stat.st_size > 65536
    {
        return Err(ExecutorError::new(
            "private database environment has invalid type, permissions or size",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(descriptor)
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| ExecutorError::new("private database environment cannot be read"))?;
    if bytes.len() > 65536 {
        return Err(ExecutorError::new(
            "private database environment exceeds its bound",
        ));
    }
    let environment: BTreeMap<String, String> = serde_json::from_slice(&bytes)
        .map_err(|_| ExecutorError::new("private database environment is invalid"))?;
    if environment.keys().any(|key| {
        ![
            "PGHOST",
            "PGPORT",
            "PGUSER",
            "PGPASSWORD",
            "PGDATABASE",
            "DATABASE_URL",
        ]
        .contains(&key.as_str())
    }) {
        return Err(ExecutorError::new(
            "private database environment contains an unsupported field",
        ));
    }
    Ok(environment)
}

#[allow(clippy::too_many_arguments)]
async fn execute_check(
    consumed_expected: Result<Vec<ArtifactReceipt>, ExecutorError>,
    progress: UnboundedSender<ProcessUpdate>,
    plan: Arc<ExecutionPlan>,
    mut check: CheckPlan,
    root: PathBuf,
    current: PathBuf,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: Arc<PathBuf>,
) -> CheckOutcome {
    let started = Instant::now();
    let started_epoch_ms = epoch_ms();
    let permits = crate::capacity::for_check(permits, &plan.run_id, &check);
    let _resources = if !check.resources.is_empty() {
        let reservation = tokio::select! {
            reservation = permits.reserve(&plan.run_id, &check.name) => reservation,
            () = cancellation.cancelled() => {
                let mut outcome = artifact_input_failure(&plan, &check, "cancelled while waiting for a resource".into(), started.elapsed());
                outcome.status = LeafStatus::Cancelled;
                outcome.failures = vec![failure_entry(&plan.run_id, Some(&check.name), None, LeafStatus::Cancelled, None, Some(TerminationReason::RunCancelled), ErrorCategory::Cancellation, None)];
                return outcome;
            }
        };
        match reservation {
            Ok(reservation) => Some(reservation),
            Err(error) => {
                return artifact_input_failure(&plan, &check, error.to_string(), started.elapsed());
            }
        }
    } else {
        None
    };
    if let Some(path) = plan.environment_files.get(&check.name) {
        match read_database_environment(&current, path) {
            Ok(environment) => check.env.extend(environment),
            Err(error) => {
                return artifact_input_failure(&plan, &check, error.to_string(), started.elapsed());
            }
        }
    }
    let cache_inputs = artifact_receipts(&root, &check.cache_inputs);
    let consumed_artifacts = artifact_receipts(
        &root,
        &check
            .consumes
            .iter()
            .map(|consumed| consumed.path.clone())
            .collect::<Vec<_>>(),
    );
    let (cache_inputs, consumed_artifacts) = match (cache_inputs, consumed_artifacts) {
        (Ok(cache_inputs), Ok(consumed_artifacts)) => (cache_inputs, consumed_artifacts),
        (Err(error), _) | (_, Err(error)) => {
            return artifact_input_failure(&plan, &check, error.to_string(), started.elapsed());
        }
    };
    let cache_key = crate::reuse::key(&root, &plan, &check, &cache_inputs, &consumed_artifacts);
    if !matches!(consumed_expected, Ok(ref expected) if expected == &consumed_artifacts) {
        return artifact_input_failure(
            &plan,
            &check,
            "consumed artifact changed after its producer completed".into(),
            started.elapsed(),
        );
    }
    if let Some(artifacts) = cache_key
        .as_deref()
        .and_then(|key| crate::reuse::lookup(&root, &check, key))
    {
        let selector = LeafSelector::check(check.name.clone()).expect("validated check");
        if log_lease
            .publish_leaf_metadata(&LeafLogMetadata {
                schema: 2,
                selector,
                status: LeafStatus::Reused,
                exit: DiagnosticExit::default(),
                started_at_epoch_ms: started_epoch_ms,
                finished_at_epoch_ms: Some(epoch_ms()),
                process_started: false,
                complete: true,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .is_ok()
        {
            return CheckOutcome {
                cache_key: None,
                status: LeafStatus::Reused,
                exit_code: None,
                reason: None,
                artifacts,
                cache_inputs,
                consumed_artifacts,
                retained_artifacts: Vec::new(),
                streams: Vec::new(),
                case_count: 0,
                cases: Vec::new(),
                cases_truncated: false,
                failures: Vec::new(),
                service: None,
                duration_seconds: seconds(started.elapsed()),
            };
        }
    }
    if let Some(command) = &check.command {
        let selector = LeafSelector::check(check.name.clone()).expect("validated check selector");
        let request = process_request(
            progress.clone(),
            &plan,
            &check,
            &root,
            &current,
            log_lease.clone(),
            &log_dir,
            selector.clone(),
            &check.name,
            command.clone(),
            check.completion,
            false,
        );
        let mut process = run_process(request, permits, cancellation).await;
        finalize_leaf_evidence(
            &plan,
            &check,
            &selector,
            &log_lease,
            &log_dir,
            &mut process,
            started_epoch_ms,
        );
        let retained_artifacts = if matches!(
            process.status,
            ProcessStatus::Passed | ProcessStatus::Failed
        ) && process.process_started
            && process.exit_code.is_some()
            && process.service.is_none()
        {
            let evidence_dir = leaf_log_directory(&log_dir, &selector).join("evidence");
            match retain_artifact_trees(
                &root,
                &evidence_dir,
                &RetainedArtifactIdentity {
                    run_id: &plan.run_id,
                    test: &plan.test,
                    check: &check.name,
                    requested_tier: plan.requested_tier,
                    readiness_eligible: plan.readiness_eligible,
                    proof: plan.proof,
                    source_sha256: &plan.source_digest,
                    config_sha256: &plan.config_digest,
                },
                &check.retained_artifacts,
            ) {
                Ok(receipts) => receipts,
                Err(error) => {
                    record_leaf_postprocess_failure(
                        &plan,
                        &check,
                        &selector,
                        &log_lease,
                        &mut process,
                        started_epoch_ms,
                        LeafStatus::Failed,
                        ErrorCategory::Artifact,
                        None,
                    );
                    let _ = error;
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let artifacts = if process.status == ProcessStatus::Passed {
            match artifact_receipts(&root, &check.produces) {
                Ok(receipts) => receipts,
                Err(error) => {
                    process.status = ProcessStatus::Failed;
                    process.reason = Some(error.to_string());
                    record_leaf_postprocess_failure(
                        &plan,
                        &check,
                        &selector,
                        &log_lease,
                        &mut process,
                        started_epoch_ms,
                        LeafStatus::Failed,
                        ErrorCategory::Artifact,
                        None,
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut outcome = direct_outcome(
            process,
            artifacts,
            retained_artifacts,
            cache_inputs,
            consumed_artifacts,
            started.elapsed(),
        );
        outcome.cache_key = cache_key;
        outcome
    } else {
        let mut outcome = execute_fanout(
            progress.clone(),
            &plan,
            &check,
            &root,
            &current,
            permits,
            cancellation,
            log_lease,
            log_dir,
            started,
            started_epoch_ms,
        )
        .await;
        outcome.cache_inputs = cache_inputs;
        outcome.consumed_artifacts = consumed_artifacts;
        outcome
    }
}

fn artifact_input_failure(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    reason: String,
    duration: Duration,
) -> CheckOutcome {
    CheckOutcome {
        cache_key: None,
        status: LeafStatus::Failed,
        exit_code: None,
        reason: Some(bounded_reason(&reason)),
        artifacts: Vec::new(),
        cache_inputs: Vec::new(),
        consumed_artifacts: Vec::new(),
        retained_artifacts: Vec::new(),
        streams: Vec::new(),
        case_count: 0,
        cases: Vec::new(),
        cases_truncated: false,
        failures: vec![failure_entry(
            &plan.run_id,
            Some(&check.name),
            None,
            LeafStatus::Failed,
            None,
            None,
            ErrorCategory::Artifact,
            None,
        )],
        service: None,
        duration_seconds: seconds(duration),
    }
}

fn direct_outcome(
    mut process: ProcessResult,
    artifacts: Vec<ArtifactReceipt>,
    retained_artifacts: Vec<RetainedArtifactReceipt>,
    cache_inputs: Vec<ArtifactReceipt>,
    consumed_artifacts: Vec<ArtifactReceipt>,
    duration: Duration,
) -> CheckOutcome {
    let status: LeafStatus = process.status.into();
    let failures = std::mem::take(&mut process.diagnostics);
    CheckOutcome {
        cache_key: None,
        status,
        exit_code: process.exit_code,
        reason: process.reason.take().map(|value| bounded_reason(&value)),
        artifacts,
        cache_inputs,
        consumed_artifacts,
        retained_artifacts,
        streams: process.streams,
        case_count: 0,
        cases: Vec::new(),
        cases_truncated: false,
        failures,
        service: process.service.take(),
        duration_seconds: seconds(duration),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_fanout(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    root: &Path,
    current: &Path,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: Arc<PathBuf>,
    started: Instant,
    _started_epoch_ms: u64,
) -> CheckOutcome {
    let mut streams = Vec::new();
    let mut cases = if let Some(static_cases) = &check.cases {
        static_cases.clone()
    } else {
        let discovery_started_epoch_ms = epoch_ms();
        let selector =
            LeafSelector::discovery(check.name.clone()).expect("validated discovery selector");
        let request = process_request(
            progress.clone(),
            plan,
            check,
            root,
            current,
            log_lease.clone(),
            &log_dir,
            selector.clone(),
            &format!("{}/discovery", check.name),
            check.discover.clone().unwrap_or_default(),
            CompletionMode::Process,
            true,
        );
        let mut discovery = run_process(request, permits.clone(), cancellation.clone()).await;
        finalize_leaf_evidence(
            plan,
            check,
            &selector,
            &log_lease,
            &log_dir,
            &mut discovery,
            discovery_started_epoch_ms,
        );
        streams.extend(discovery.streams.iter().cloned());
        if discovery.status != ProcessStatus::Passed {
            return fanout_setup_failure(
                &plan.run_id,
                check,
                discovery,
                streams,
                started.elapsed(),
            );
        }
        let Some(manifest_capture) = discovery.manifest.take() else {
            return discovery_postprocess_failure(
                plan,
                check,
                &selector,
                &log_lease,
                discovery,
                discovery_started_epoch_ms,
                streams,
                started.elapsed(),
                LeafStatus::Failed,
                "case discovery closed without a descriptor manifest".into(),
                ErrorCategory::StructuredEvidenceInvalid,
                None,
            );
        };
        if manifest_capture.truncated || manifest_capture.observed > MAX_MANIFEST_BYTES as u64 {
            return discovery_postprocess_failure(
                plan,
                check,
                &selector,
                &log_lease,
                discovery,
                discovery_started_epoch_ms,
                streams,
                started.elapsed(),
                LeafStatus::Failed,
                "case manifest descriptor exceeds 2 MiB".into(),
                ErrorCategory::StructuredEvidenceInvalid,
                None,
            );
        }
        if manifest_capture.payload.is_empty() {
            return discovery_postprocess_failure(
                plan,
                check,
                &selector,
                &log_lease,
                discovery,
                discovery_started_epoch_ms,
                streams,
                started.elapsed(),
                LeafStatus::Failed,
                "case discovery descriptor closed without a manifest".into(),
                ErrorCategory::StructuredEvidenceInvalid,
                None,
            );
        }
        let path = current
            .join("checks")
            .join(&check.name)
            .join("manifest.json");
        if let Err(error) = write_bytes_atomic(&path, &manifest_capture.payload, MAX_MANIFEST_BYTES)
        {
            return discovery_postprocess_failure(
                plan,
                check,
                &selector,
                &log_lease,
                discovery,
                discovery_started_epoch_ms,
                streams,
                started.elapsed(),
                LeafStatus::Unsafe,
                error.to_string(),
                ErrorCategory::LogStorage,
                Some(TerminationReason::UnsafeStop),
            );
        }
        match CaseManifest::from_json(&manifest_capture.payload) {
            Ok(manifest) => manifest.cases,
            Err(error) => {
                return discovery_postprocess_failure(
                    plan,
                    check,
                    &selector,
                    &log_lease,
                    discovery,
                    discovery_started_epoch_ms,
                    streams,
                    started.elapsed(),
                    LeafStatus::Failed,
                    error.to_string(),
                    ErrorCategory::StructuredEvidenceInvalid,
                    None,
                );
            }
        }
    };
    cases.sort_by(|left, right| left.id.cmp(&right.id));
    if let Some(selected) = plan.case_selection.get(&check.name) {
        let available = cases
            .iter()
            .map(|case| case.id.as_str())
            .collect::<BTreeSet<_>>();
        if selected
            .iter()
            .any(|case| !available.contains(case.as_str()))
        {
            return case_selection_failure(
                plan,
                check,
                streams,
                started.elapsed(),
                "selected case was not reported by discovery",
            );
        }
        let selected = selected.iter().map(String::as_str).collect::<BTreeSet<_>>();
        cases.retain(|case| selected.contains(case.id.as_str()));
    }
    if plan.fixture_program.is_none()
        && cases.iter().any(|case| {
            case.postgres
                .as_ref()
                .is_some_and(|branch| !plan.postgres_databases.contains_key(branch))
        })
    {
        return case_selection_failure(
            plan,
            check,
            streams,
            started.elapsed(),
            "case references an unavailable PostgreSQL branch",
        );
    }
    let case_command = check.case_command.clone().unwrap_or_default();
    let mut tasks = JoinSet::<CaseOutcome>::new();
    for case in cases {
        let plan = plan.clone();
        let check = check.clone();
        let root = root.to_path_buf();
        let current = current.to_path_buf();
        let permits = permits.clone();
        let cancellation = cancellation.clone();
        let case_command = case_command.clone();
        let log_lease = log_lease.clone();
        let log_dir = log_dir.clone();
        let progress = progress.clone();
        tasks.spawn(async move {
            run_case(
                progress.clone(),
                &plan,
                &check,
                &case,
                &case_command,
                &root,
                &current,
                permits,
                cancellation,
                log_lease,
                log_dir,
            )
            .await
        });
    }
    let mut outcomes = Vec::new();
    let mut join_failure = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => join_failure = Some(format!("case task failed internally: {error}")),
        }
    }
    outcomes.sort_by(|left, right| left.report.id.cmp(&right.report.id));
    let mut failures = Vec::new();
    for outcome in &outcomes {
        failures.extend(outcome.failures.iter().cloned());
    }
    let case_reports: Vec<CaseReport> = outcomes.into_iter().map(|row| row.report).collect();
    let (status, reason) = if let Some(reason) = join_failure {
        (LeafStatus::Unsafe, Some(reason))
    } else if case_reports
        .iter()
        .any(|case| case.status == LeafStatus::Unsafe)
    {
        (
            LeafStatus::Unsafe,
            Some("one or more cases became unsafe".into()),
        )
    } else if case_reports
        .iter()
        .any(|case| case.status == LeafStatus::TimedOut)
    {
        (
            LeafStatus::TimedOut,
            Some("one or more cases timed out".into()),
        )
    } else if case_reports
        .iter()
        .any(|case| case.status == LeafStatus::Cancelled)
    {
        (
            LeafStatus::Cancelled,
            Some("one or more cases were cancelled".into()),
        )
    } else if case_reports
        .iter()
        .any(|case| case.status == LeafStatus::Failed)
    {
        (LeafStatus::Failed, Some("one or more cases failed".into()))
    } else {
        (LeafStatus::Passed, None)
    };
    if status == LeafStatus::Unsafe && failures.is_empty() {
        failures.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            None,
            status,
            None,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::Internal,
            None,
        ));
    }
    let mut final_status = status;
    let mut final_reason = reason;
    let case_count = u32::try_from(case_reports.len()).unwrap_or(u32::MAX);
    let case_evidence = serde_json::json!({
        "schema": 2,
        "check": check.name,
        "cases": &case_reports,
    });
    let evidence_result = serde_json::to_vec(&case_evidence)
        .map_err(|error| ExecutorError::new(format!("cannot encode case evidence: {error}")))
        .and_then(|payload| {
            write_bytes_atomic(
                &current
                    .join("checks")
                    .join(&check.name)
                    .join("case-report.json"),
                &payload,
                MAX_CASE_EVIDENCE_BYTES,
            )
        });
    if let Err(error) = evidence_result {
        final_status = LeafStatus::Unsafe;
        final_reason = Some(error.to_string());
        failures.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            None,
            LeafStatus::Unsafe,
            None,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::LogStorage,
            None,
        ));
    }
    let artifacts = if final_status == LeafStatus::Passed {
        match artifact_receipts(root, &check.produces) {
            Ok(receipts) => receipts,
            Err(error) => {
                final_status = LeafStatus::Failed;
                final_reason = Some(error.to_string());
                failures.push(failure_entry(
                    &plan.run_id,
                    Some(&check.name),
                    None,
                    LeafStatus::Failed,
                    None,
                    None,
                    ErrorCategory::Artifact,
                    None,
                ));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    CheckOutcome {
        cache_key: None,
        status: final_status,
        exit_code: None,
        reason: final_reason.map(|value| bounded_reason(&value)),
        artifacts,
        cache_inputs: Vec::new(),
        consumed_artifacts: Vec::new(),
        retained_artifacts: Vec::new(),
        streams,
        case_count,
        cases: case_reports
            .iter()
            .take(MAX_INLINE_CASES)
            .cloned()
            .collect(),
        cases_truncated: case_reports.len() > MAX_INLINE_CASES,
        failures,
        service: None,
        duration_seconds: seconds(started.elapsed()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_case(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    case: &CaseSpec,
    base_command: &[String],
    root: &Path,
    current: &Path,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: Arc<PathBuf>,
) -> CaseOutcome {
    if plan.fixture_program.is_some() && case.postgres.is_some() {
        return run_case_with_fixture(
            progress,
            plan,
            check,
            case,
            base_command,
            root,
            current,
            permits,
            cancellation,
            log_lease,
            log_dir,
        )
        .await;
    }
    run_plain_case(
        progress,
        plan,
        check,
        case,
        base_command,
        root,
        current,
        permits,
        cancellation,
        log_lease,
        log_dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_plain_case(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    case: &CaseSpec,
    base_command: &[String],
    root: &Path,
    current: &Path,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: Arc<PathBuf>,
) -> CaseOutcome {
    let started = Instant::now();
    let started_epoch_ms = epoch_ms();
    let mut command = base_command.to_vec();
    command.extend(case.args.clone());
    let leaf = format!("{}/case/{}", check.name, case.id);
    let selector =
        LeafSelector::case(check.name.clone(), case.id.clone()).expect("validated case selector");
    let mut request = process_request(
        progress.clone(),
        plan,
        check,
        root,
        current,
        log_lease.clone(),
        &log_dir,
        selector.clone(),
        &leaf,
        command,
        CompletionMode::Process,
        false,
    );
    if let Some(branch) = &case.postgres {
        let key = format!("{}-{}", check.name, case.id);
        let database = plan
            .postgres_databases
            .get(&key)
            .or_else(|| plan.postgres_databases.get(branch))
            .ok_or_else(|| ExecutorError::new("case references an unavailable PostgreSQL branch"));
        let database = match database {
            Ok(value) => value,
            Err(_error) => {
                return CaseOutcome {
                    report: CaseReport {
                        phases: Vec::new(),
                        execution: None,
                        id: case.id.clone(),
                        status: LeafStatus::Unsafe,
                        exit: diagnostic_exit(None),
                        duration_ms: millis(started.elapsed()),
                        streams: Vec::new(),
                    },
                    failures: vec![failure_entry(
                        &plan.run_id,
                        Some(&check.name),
                        Some(&case.id),
                        LeafStatus::Unsafe,
                        None,
                        Some(TerminationReason::UnsafeStop),
                        ErrorCategory::Dependency,
                        None,
                    )],
                };
            }
        };
        request.env.insert("PGDATABASE".into(), database.clone());
        if let Some(url) = request.env.get_mut("DATABASE_URL") {
            *url = database_url_with_database(url, database);
        }
    }
    if let Some(file) = plan
        .environment_files
        .get(&format!("{}/{}", check.name, case.id))
    {
        let bytes = match std::fs::read(current.join(file)) {
            Ok(bytes) => bytes,
            Err(error) => {
                return database_case_failure(
                    plan,
                    check,
                    case,
                    format!("private case database environment unavailable: {error}"),
                    started.elapsed(),
                );
            }
        };
        let environment = match serde_json::from_slice::<BTreeMap<String, String>>(&bytes) {
            Ok(environment) => environment,
            Err(error) => {
                return database_case_failure(
                    plan,
                    check,
                    case,
                    format!("private case database environment invalid: {error}"),
                    started.elapsed(),
                );
            }
        };
        request.env.extend(environment);
    }
    let mut process = run_process(request, permits, cancellation).await;
    finalize_leaf_evidence(
        plan,
        check,
        &selector,
        &log_lease,
        &log_dir,
        &mut process,
        started_epoch_ms,
    );
    CaseOutcome {
        report: CaseReport {
            phases: Vec::new(),
            execution: None,
            id: case.id.clone(),
            status: process.status.into(),
            exit: diagnostic_exit(process.exit_code),
            duration_ms: millis(started.elapsed()),
            streams: process.streams,
        },
        failures: process.diagnostics,
    }
}

#[allow(clippy::too_many_arguments)]
async fn native_case_phase(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    case: &CaseSpec,
    phase: LogPhase,
    root: &Path,
    current: &Path,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: &Path,
) -> (
    ProcessResult,
    devcoordinator2_executor_protocol::CasePhaseReport,
) {
    let stage = if phase == LogPhase::Fixture {
        "fixture"
    } else {
        "cleanup"
    };
    let node = format!(
        "{}/{stage}/{}/{}",
        check.name,
        case.id,
        case.postgres.as_deref().unwrap()
    );
    let selector = LeafSelector::new(Some(check.name.clone()), phase, Some(case.id.clone()))
        .expect("validated fixture case");
    let command = vec![
        plan.fixture_program.clone().unwrap(),
        "fixture".into(),
        node.clone(),
    ];
    let request = process_request(
        progress,
        plan,
        check,
        root,
        current,
        log_lease.clone(),
        log_dir,
        selector.clone(),
        &node,
        command,
        CompletionMode::Process,
        false,
    );
    let started = Instant::now();
    let at = epoch_ms();
    let mut process = run_process(request, permits, cancellation).await;
    let mut native = check.clone();
    native.diagnostic_sources.clear();
    native.expect_failure = false;
    native.expected_failure = None;
    native.expected_exit_code = None;
    finalize_leaf_evidence(
        plan,
        &native,
        &selector,
        &log_lease,
        log_dir,
        &mut process,
        at,
    );
    let report = devcoordinator2_executor_protocol::CasePhaseReport {
        phase: if phase == LogPhase::Fixture {
            devcoordinator2_executor_protocol::CheckPhase::Fixture
        } else {
            devcoordinator2_executor_protocol::CheckPhase::Cleanup
        },
        status: process.status.into(),
        duration_ms: millis(started.elapsed()),
        streams: process.streams.clone(),
    };
    (process, report)
}

#[allow(clippy::too_many_arguments)]
async fn run_case_with_fixture(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    case: &CaseSpec,
    base_command: &[String],
    root: &Path,
    current: &Path,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    log_lease: Arc<RunLogLease>,
    log_dir: Arc<PathBuf>,
) -> CaseOutcome {
    use devcoordinator2_executor_protocol::{CasePhaseReport, CheckPhase};
    let started = Instant::now();
    let (setup, setup_phase) = native_case_phase(
        progress.clone(),
        plan,
        check,
        case,
        LogPhase::Fixture,
        root,
        current,
        permits.clone(),
        cancellation.clone(),
        log_lease.clone(),
        &log_dir,
    )
    .await;
    let mut ran_body = false;
    let mut outcome = if setup.status == ProcessStatus::Passed {
        let environment_file = format!(
            "database-{}.json",
            crate::case_environment_key(&check.name, &case.id)
        );
        match read_database_environment(current, &environment_file) {
            Ok(environment) => {
                let mut isolated_check = check.clone();
                isolated_check.env.extend(environment);
                let mut plain_case = case.clone();
                plain_case.postgres = None;
                ran_body = true;
                run_plain_case(
                    progress.clone(),
                    plan,
                    &isolated_check,
                    &plain_case,
                    base_command,
                    root,
                    current,
                    permits.clone(),
                    cancellation.clone(),
                    log_lease.clone(),
                    log_dir.clone(),
                )
                .await
            }
            Err(error) => {
                database_case_failure(plan, check, case, error.to_string(), started.elapsed())
            }
        }
    } else {
        CaseOutcome {
            report: CaseReport {
                phases: Vec::new(),
                execution: None,
                id: case.id.clone(),
                status: setup.status.into(),
                exit: DiagnosticExit::default(),
                duration_ms: 0,
                streams: Vec::new(),
            },
            failures: Vec::new(),
        }
    };
    let body_phase = CasePhaseReport {
        phase: CheckPhase::Case,
        status: if ran_body {
            outcome.report.status
        } else {
            LeafStatus::Invalidated
        },
        duration_ms: if ran_body {
            outcome.report.duration_ms
        } else {
            0
        },
        streams: outcome.report.streams.clone(),
    };
    let (cleanup, cleanup_phase) = native_case_phase(
        progress,
        plan,
        check,
        case,
        LogPhase::Cleanup,
        root,
        current,
        permits,
        Cancellation::default(),
        log_lease,
        &log_dir,
    )
    .await;
    outcome.failures.extend(setup.diagnostics);
    outcome.failures.extend(cleanup.diagnostics);
    if !cleanup_phase.status.is_success() && outcome.report.status.is_success() {
        outcome.report.status = cleanup_phase.status;
    }
    outcome.report.phases = vec![setup_phase, body_phase, cleanup_phase];
    outcome.report.duration_ms = millis(started.elapsed());
    outcome
}

fn database_case_failure(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    case: &CaseSpec,
    _reason: String,
    duration: Duration,
) -> CaseOutcome {
    CaseOutcome {
        report: CaseReport {
            phases: Vec::new(),
            execution: None,
            id: case.id.clone(),
            status: LeafStatus::Unsafe,
            exit: diagnostic_exit(None),
            duration_ms: millis(duration),
            streams: Vec::new(),
        },
        failures: vec![failure_entry(
            &plan.run_id,
            Some(&check.name),
            Some(&case.id),
            LeafStatus::Unsafe,
            None,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::Artifact,
            None,
        )],
    }
}

fn database_url_with_database(url: &str, database: &str) -> String {
    let (base, suffix) = url
        .split_once('?')
        .map_or((url, ""), |(base, query)| (base, query));
    let Some(slash) = base.rfind('/') else {
        return url.to_owned();
    };
    let mut replaced = format!("{}{database}", &base[..=slash]);
    if !suffix.is_empty() {
        replaced.push('?');
        replaced.push_str(suffix);
    }
    replaced
}

#[allow(clippy::too_many_arguments)]
fn process_request(
    progress: UnboundedSender<ProcessUpdate>,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    root: &Path,
    current: &Path,
    log_lease: Arc<RunLogLease>,
    log_dir: &Path,
    log_selector: LeafSelector,
    leaf: &str,
    command: Vec<String>,
    completion: CompletionMode,
    capture_manifest: bool,
) -> ProcessRequest {
    let scratch = if let Some(case_id) = &log_selector.case_id {
        let directory = current
            .join("scratch")
            .join(&check.name)
            .join("cases")
            .join(case_id);
        match log_selector.phase {
            LogPhase::Fixture => directory.join("fixture"),
            LogPhase::Cleanup => directory.join("cleanup"),
            _ => directory,
        }
    } else {
        current.join("scratch").join(&check.name)
    };
    let diagnostics_dir = leaf_log_directory(log_dir, &log_selector).join("diagnostics");
    let evidence_dir = leaf_log_directory(log_dir, &log_selector).join("evidence");
    ProcessRequest {
        progress,
        run_id: plan.run_id.clone(),
        check_name: check.name.clone(),
        leaf_id: leaf.into(),
        command,
        cwd: root.join(&check.cwd),
        env: check.env.clone(),
        scratch,
        shared_artifacts: current.join("artifacts"),
        diagnostics_dir,
        evidence_dir,
        log_lease,
        log_selector,
        timeout_seconds: check.timeout_seconds,
        completion,
        capture_manifest,
    }
}

fn finalize_leaf_evidence(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    selector: &LeafSelector,
    log_lease: &RunLogLease,
    log_dir: &Path,
    process: &mut ProcessResult,
    started_at_epoch_ms: u64,
) {
    if process.service.is_some() {
        return;
    }
    supplement_stream_summaries(plan, log_dir, selector, &mut process.streams);
    if process.process_started
        && (process.streams.len() != 2 || process.streams.iter().any(|stream| !stream.complete))
    {
        process.status = ProcessStatus::Unsafe;
        process.log_storage_failed = true;
        process.reason = Some("log storage became incomplete".into());
    }
    let context = DiagnosticContext {
        run_id: plan.run_id.clone(),
        check: check.name.clone(),
        case: selector.case_id.clone(),
        phase: selector.phase,
    };
    let mut formats = Vec::new();
    let mut invalid = process.structured_evidence_invalid;
    for source in &check.diagnostic_sources {
        match read_declared_diagnostics(log_lease, selector, source, &context) {
            Ok(entries) => {
                formats.push(source.format);
                process.diagnostics.extend(entries);
            }
            Err(()) => invalid = true,
        }
    }
    if !process.diagnostics.is_empty() && process.status == ProcessStatus::Passed {
        process.status = ProcessStatus::Failed;
    }
    if invalid {
        process.status = ProcessStatus::Unsafe;
        process.structured_evidence_invalid = true;
        process.reason = Some("structured diagnostic evidence is invalid".into());
        process.diagnostics.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            selector.case_id.as_deref(),
            LeafStatus::Unsafe,
            process.exit_code,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::StructuredEvidenceInvalid,
            Some(selector.phase),
        ));
    }
    if check.expect_failure && !invalid && !process.log_storage_failed {
        match process.status {
            ProcessStatus::Failed
                if process.process_started
                    && process.exit_code == Some(check.expected_exit_code.unwrap_or(1))
                    && process
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.origin != DiagnosticOrigin::Executor)
                    && process.diagnostics.iter().all(|diagnostic| {
                        if diagnostic.origin == DiagnosticOrigin::Executor {
                            return diagnostic.error_category == ErrorCategory::ProcessExit
                                && diagnostic.exit.code == process.exit_code
                                && diagnostic.exit.signal.is_none()
                                && diagnostic.termination_reason.is_none();
                        }
                        let mut canonical = diagnostic.clone();
                        if let Some(source_name) = &check.source_name {
                            canonical.check = Some(source_name.clone());
                        }
                        check.expected_failure.as_deref()
                            == Some(crate::diagnostics::diagnostic_fingerprint(&canonical).as_str())
                    }) =>
            {
                process.status = ProcessStatus::Passed;
                process.reason = None;
                process.diagnostics.clear();
            }
            ProcessStatus::Passed => {
                process.status = ProcessStatus::Failed;
                process.reason = Some("known failing fixture was not detected".into());
            }
            ProcessStatus::Failed => {
                process.reason = Some("qualification failed for an unexpected diagnostic".into());
            }
            ProcessStatus::TimedOut | ProcessStatus::Cancelled | ProcessStatus::Unsafe => {}
        }
    }
    let status: LeafStatus = process.status.into();
    if !status.is_success()
        && !process
            .diagnostics
            .iter()
            .any(|entry| entry.origin == DiagnosticOrigin::Executor)
    {
        let (termination_reason, error_category) =
            process_failure_semantics(process, check.completion);
        process.diagnostics.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            selector.case_id.as_deref(),
            status,
            process.exit_code,
            termination_reason,
            error_category,
            Some(selector.phase),
        ));
    }
    match normalize_diagnostics(std::mem::take(&mut process.diagnostics)) {
        Ok(normalized) => process.diagnostics = normalized.entries,
        Err(_) => {
            process.status = ProcessStatus::Unsafe;
            process.structured_evidence_invalid = true;
            process.diagnostics = vec![failure_entry(
                &plan.run_id,
                Some(&check.name),
                selector.case_id.as_deref(),
                LeafStatus::Unsafe,
                process.exit_code,
                Some(TerminationReason::UnsafeStop),
                ErrorCategory::StructuredEvidenceInvalid,
                Some(selector.phase),
            )];
        }
    }
    if log_lease
        .publish_leaf_diagnostics(selector, &process.diagnostics)
        .is_err()
    {
        process.status = ProcessStatus::Unsafe;
        process.log_storage_failed = true;
        process.reason = Some("cannot publish structured diagnostic evidence".into());
        process.diagnostics.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            selector.case_id.as_deref(),
            LeafStatus::Unsafe,
            process.exit_code,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::LogStorage,
            Some(selector.phase),
        ));
    }
    formats.sort();
    formats.dedup();
    let complete = !process.log_storage_failed
        && (!process.process_started
            || (process.streams.len() == 2
                && process.streams.iter().all(|stream| stream.complete)));
    let metadata = LeafLogMetadata {
        schema: 2,
        selector: selector.clone(),
        status: process.status.into(),
        exit: diagnostic_exit(process.exit_code),
        started_at_epoch_ms,
        finished_at_epoch_ms: Some(epoch_ms()),
        process_started: process.process_started,
        complete,
        structured_evidence_formats: formats,
        structured_evidence_count: process.diagnostics.len() as u64,
    };
    if log_lease.publish_leaf_metadata(&metadata).is_err() {
        process.status = ProcessStatus::Unsafe;
        process.log_storage_failed = true;
        process.reason = Some("cannot publish leaf log metadata".into());
        process.diagnostics.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            selector.case_id.as_deref(),
            LeafStatus::Unsafe,
            process.exit_code,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::LogStorage,
            Some(selector.phase),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn record_leaf_postprocess_failure(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    selector: &LeafSelector,
    log_lease: &RunLogLease,
    process: &mut ProcessResult,
    started_at_epoch_ms: u64,
    status: LeafStatus,
    error_category: ErrorCategory,
    termination_reason: Option<TerminationReason>,
) {
    process.status = if status == LeafStatus::Unsafe {
        ProcessStatus::Unsafe
    } else {
        ProcessStatus::Failed
    };
    if error_category == ErrorCategory::LogStorage {
        process.log_storage_failed = true;
    }
    if error_category == ErrorCategory::StructuredEvidenceInvalid {
        process.structured_evidence_invalid = true;
    }
    process.diagnostics.push(failure_entry(
        &plan.run_id,
        Some(&check.name),
        selector.case_id.as_deref(),
        status,
        process.exit_code,
        termination_reason,
        error_category,
        Some(selector.phase),
    ));
    if let Ok(normalized) = normalize_diagnostics(std::mem::take(&mut process.diagnostics)) {
        process.diagnostics = normalized.entries;
    } else {
        process.structured_evidence_invalid = true;
    }
    let mut metadata_failed = log_lease
        .publish_leaf_diagnostics(selector, &process.diagnostics)
        .is_err();
    let mut formats: Vec<_> = check
        .diagnostic_sources
        .iter()
        .map(|source| source.format)
        .collect();
    formats.sort();
    formats.dedup();
    let complete = !process.log_storage_failed
        && (!process.process_started
            || (process.streams.len() == 2
                && process.streams.iter().all(|stream| stream.complete)));
    metadata_failed |= log_lease
        .publish_leaf_metadata(&LeafLogMetadata {
            schema: 2,
            selector: selector.clone(),
            status,
            exit: diagnostic_exit(process.exit_code),
            started_at_epoch_ms,
            finished_at_epoch_ms: Some(epoch_ms()),
            process_started: process.process_started,
            complete,
            structured_evidence_formats: formats,
            structured_evidence_count: process.diagnostics.len() as u64,
        })
        .is_err();
    if metadata_failed {
        process.status = ProcessStatus::Unsafe;
        process.log_storage_failed = true;
        process.reason = Some("cannot publish terminal leaf evidence".into());
        process.diagnostics.push(failure_entry(
            &plan.run_id,
            Some(&check.name),
            selector.case_id.as_deref(),
            LeafStatus::Unsafe,
            process.exit_code,
            Some(TerminationReason::UnsafeStop),
            ErrorCategory::LogStorage,
            Some(selector.phase),
        ));
    }
}

fn supplement_stream_summaries(
    plan: &ExecutionPlan,
    log_dir: &Path,
    selector: &LeafSelector,
    streams: &mut Vec<LogStreamSummary>,
) {
    let leaf_dir = leaf_log_directory(log_dir, selector);
    for stream in [LogStream::Stdout, LogStream::Stderr] {
        if streams
            .iter()
            .any(|summary| summary.log_ref.stream == stream)
        {
            continue;
        }
        let name = match stream {
            LogStream::Stdout => "stdout.meta.json",
            LogStream::Stderr => "stderr.meta.json",
        };
        let Ok(payload) = fs::read(leaf_dir.join(name)) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_slice::<StreamMetadata>(&payload) else {
            continue;
        };
        if metadata.selector != *selector || metadata.stream != stream {
            continue;
        }
        streams.push(LogStreamSummary {
            log_ref: LogRef {
                run_id: plan.run_id.clone(),
                check: selector.check.clone(),
                phase: selector.phase,
                case: selector.case_id.clone(),
                stream,
            },
            bytes: metadata.bytes,
            lines: metadata.lines,
            sha256: metadata.sha256,
            first_write_epoch_ms: metadata.first_write_epoch_ms,
            last_write_epoch_ms: metadata.last_write_epoch_ms,
            complete: metadata.complete,
        });
    }
    streams.sort_by_key(|summary| summary.log_ref.stream);
}

fn read_declared_diagnostics(
    log_lease: &RunLogLease,
    selector: &LeafSelector,
    source: &DiagnosticReportSource,
    context: &DiagnosticContext,
) -> Result<Vec<FailureIndexEntry>, ()> {
    let relative = format!("diagnostics/{}", source.path);
    let input = log_lease
        .read_declared_report(selector, &relative, MAX_DECLARED_DIAGNOSTIC_REPORT_BYTES)
        .map_err(|_| ())?;
    parse_declared_report(source, &input, context)
        .map(|parsed| parsed.entries)
        .map_err(|_| ())
}

fn leaf_log_directory(run_dir: &Path, selector: &LeafSelector) -> PathBuf {
    match selector.phase {
        LogPhase::Executor => run_dir.join("executor"),
        LogPhase::Check | LogPhase::Discovery => run_dir
            .join("checks")
            .join(selector.check.as_deref().expect("validated check selector"))
            .join(match selector.phase {
                LogPhase::Check => "check",
                LogPhase::Discovery => "discovery",
                _ => unreachable!(),
            }),
        LogPhase::Case | LogPhase::Fixture | LogPhase::Cleanup => run_dir
            .join("checks")
            .join(selector.check.as_deref().expect("validated case selector"))
            .join(match selector.phase {
                LogPhase::Fixture => "fixtures",
                LogPhase::Cleanup => "cleanup",
                _ => "cases",
            })
            .join(
                selector
                    .case_id
                    .as_deref()
                    .expect("validated case selector"),
            ),
    }
}

fn fanout_setup_failure(
    _run_id: &str,
    _check: &CheckPlan,
    mut process: ProcessResult,
    streams: Vec<LogStreamSummary>,
    duration: Duration,
) -> CheckOutcome {
    let status: LeafStatus = process.status.into();
    let exit_code = process.exit_code;
    let reason = process
        .reason
        .unwrap_or_else(|| "case discovery failed".into());
    CheckOutcome {
        cache_key: None,
        status,
        exit_code,
        reason: Some(bounded_reason(&reason)),
        artifacts: Vec::new(),
        cache_inputs: Vec::new(),
        consumed_artifacts: Vec::new(),
        retained_artifacts: Vec::new(),
        streams,
        case_count: 0,
        cases: Vec::new(),
        cases_truncated: false,
        failures: std::mem::take(&mut process.diagnostics),
        service: None,
        duration_seconds: seconds(duration),
    }
}

fn case_selection_failure(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    streams: Vec<LogStreamSummary>,
    duration: Duration,
    reason: &str,
) -> CheckOutcome {
    CheckOutcome {
        cache_key: None,
        status: LeafStatus::Failed,
        exit_code: None,
        reason: Some(reason.into()),
        artifacts: Vec::new(),
        cache_inputs: Vec::new(),
        consumed_artifacts: Vec::new(),
        retained_artifacts: Vec::new(),
        streams,
        case_count: 0,
        cases: Vec::new(),
        cases_truncated: false,
        failures: vec![failure_entry(
            &plan.run_id,
            Some(&check.name),
            None,
            LeafStatus::Failed,
            None,
            None,
            ErrorCategory::Dependency,
            None,
        )],
        service: None,
        duration_seconds: seconds(duration),
    }
}

#[allow(clippy::too_many_arguments)]
fn discovery_postprocess_failure(
    plan: &ExecutionPlan,
    check: &CheckPlan,
    selector: &LeafSelector,
    log_lease: &RunLogLease,
    mut process: ProcessResult,
    started_at_epoch_ms: u64,
    streams: Vec<LogStreamSummary>,
    duration: Duration,
    status: LeafStatus,
    reason: String,
    error_category: ErrorCategory,
    termination_reason: Option<TerminationReason>,
) -> CheckOutcome {
    record_leaf_postprocess_failure(
        plan,
        check,
        selector,
        log_lease,
        &mut process,
        started_at_epoch_ms,
        status,
        error_category,
        termination_reason,
    );
    CheckOutcome {
        cache_key: None,
        status: process.status.into(),
        exit_code: process.exit_code,
        reason: Some(bounded_reason(&reason)),
        artifacts: Vec::new(),
        cache_inputs: Vec::new(),
        consumed_artifacts: Vec::new(),
        retained_artifacts: Vec::new(),
        streams,
        case_count: 0,
        cases: Vec::new(),
        cases_truncated: false,
        failures: process.diagnostics,
        service: None,
        duration_seconds: seconds(duration),
    }
}

fn apply_outcome(
    runtime: &mut CheckRuntime,
    mut outcome: CheckOutcome,
    services: &mut JoinSet<(String, ServiceExit)>,
    service_pgids: &mut BTreeMap<String, i32>,
) {
    runtime.completed_mono = Some(Instant::now());
    runtime.cache_key = outcome.cache_key.take();
    runtime.report.resource_waiting = false;
    runtime.report.status = outcome.status;
    runtime.report.finished_at = Some(iso_now());
    runtime.report.duration_seconds = if runtime.plan.completion == CompletionMode::Event {
        runtime
            .processes
            .values()
            .filter_map(|update| update.started_mono)
            .min()
            .map(|started| seconds(started.elapsed()))
    } else {
        Some(outcome.duration_seconds)
    };
    runtime.report.exit = diagnostic_exit(outcome.exit_code);
    let _ = outcome.reason;
    runtime.report.artifacts = outcome.artifacts;
    runtime.report.cache_inputs = outcome.cache_inputs;
    runtime.report.consumed_artifacts = outcome.consumed_artifacts;
    runtime.report.retained_artifacts = outcome.retained_artifacts;
    runtime.report.streams = outcome.streams;
    runtime.report.case_count = outcome.case_count;
    runtime.report.cases = outcome.cases;
    runtime.report.cases_truncated = outcome.cases_truncated;
    runtime.failures = outcome.failures;
    if let Some(service) = outcome.service.take() {
        let name = runtime.plan.name.clone();
        service_pgids.insert(name.clone(), service.pgid);
        services.spawn(async move { (name, service.future.await) });
    }
}

fn mark_service_exit(
    checks: &mut [CheckRuntime],
    plan: &ExecutionPlan,
    log_lease: &RunLogLease,
    log_dir: &Path,
    name: &str,
    exit: ServiceExit,
    cleanup: bool,
) -> Result<(), ExecutorError> {
    let index = check_index(checks, name)?;
    let runtime = &mut checks[index];
    let storage_failed = exit.log_storage_failed;
    let evidence_invalid = exit.structured_evidence_invalid;
    let mut process = ProcessResult {
        status: if !cleanup || storage_failed || evidence_invalid {
            ProcessStatus::Unsafe
        } else {
            ProcessStatus::Passed
        },
        exit_code: exit.exit_code,
        reason: None,
        streams: exit.streams,
        diagnostics: exit.diagnostics,
        log_storage_failed: storage_failed,
        structured_evidence_invalid: evidence_invalid,
        process_started: true,
        service: None,
        manifest: None,
        capacity: Default::default(),
    };
    let selector = LeafSelector::check(name.to_owned()).expect("validated service selector");
    finalize_leaf_evidence(
        plan,
        &runtime.plan,
        &selector,
        log_lease,
        log_dir,
        &mut process,
        runtime.started_epoch_ms.unwrap_or_else(epoch_ms),
    );
    runtime.report.exit = diagnostic_exit(process.exit_code);
    runtime.report.streams = process.streams;
    runtime.failures.extend(process.diagnostics);
    if process.status != ProcessStatus::Passed {
        runtime.report.status = process.status.into();
    }
    if !cleanup {
        runtime.report.status = LeafStatus::Unsafe;
    }
    if process.log_storage_failed || process.structured_evidence_invalid {
        runtime.report.status = LeafStatus::Unsafe;
    }
    Ok(())
}

async fn stop_services(
    services: &mut JoinSet<(String, ServiceExit)>,
    service_pgids: &mut BTreeMap<String, i32>,
    checks: &mut [CheckRuntime],
    plan: &ExecutionPlan,
    log_lease: &RunLogLease,
    log_dir: &Path,
) -> Result<(), ExecutorError> {
    for pgid in service_pgids.values().copied() {
        signal_group(pgid, Signal::TERM)?;
    }
    let deadline = tokio::time::Instant::now() + SERVICE_TERMINATION_GRACE;
    while !services.is_empty() {
        match tokio::time::timeout_at(deadline, services.join_next()).await {
            Ok(Some(joined)) => {
                let (name, exit) = joined.map_err(|error| {
                    ExecutorError::new(format!("event service task failed: {error}"))
                })?;
                service_pgids.remove(&name);
                mark_service_exit(checks, plan, log_lease, log_dir, &name, exit, true)?;
            }
            Ok(None) => break,
            Err(_) => {
                for pgid in service_pgids.values().copied() {
                    signal_group(pgid, Signal::KILL)?;
                }
                while let Some(joined) = services.join_next().await {
                    let (name, exit) = joined.map_err(|error| {
                        ExecutorError::new(format!("event service task failed: {error}"))
                    })?;
                    service_pgids.remove(&name);
                    mark_service_exit(checks, plan, log_lease, log_dir, &name, exit, true)?;
                }
                break;
            }
        }
    }
    Ok(())
}

fn mark_pending_cancelled(
    checks: &mut [CheckRuntime],
    run_id: &str,
    log_lease: &RunLogLease,
    reason: &str,
    retain_cleanup: bool,
) {
    for check in checks {
        if check.report.status == LeafStatus::Pending
            && !(retain_cleanup
                && check.plan.phase == devcoordinator2_executor_protocol::CheckPhase::Cleanup)
        {
            finish_blocked(
                check,
                run_id,
                log_lease,
                LeafStatus::Cancelled,
                reason.into(),
            );
        }
    }
}

fn abort_reason(abort: Option<&Abort>) -> &str {
    match abort {
        Some(Abort::Stopped(reason) | Abort::Unsafe(reason)) => reason,
        Some(Abort::Cancelled) | None => "run cancelled",
    }
}

#[allow(clippy::too_many_arguments)]
fn write_report(
    path: &Path,
    observations: &watch::Sender<Option<ExecutionReport>>,
    plan: &ExecutionPlan,
    checks: &[CheckRuntime],
    capacity: &Mutex<CapacityReport>,
    started_at: &str,
    started: Instant,
    status: RunStatus,
    finished_at: Option<String>,
    source_changed: bool,
    source_error: Option<String>,
) -> Result<ExecutionReport, ExecutorError> {
    let mut failures = Vec::new();
    for check in checks {
        failures.extend(check.failures.iter().cloned());
    }
    if source_changed {
        let unsafe_failure = source_error.is_some();
        failures.push(failure_entry(
            &plan.run_id,
            None,
            None,
            if unsafe_failure {
                LeafStatus::Unsafe
            } else {
                LeafStatus::Failed
            },
            None,
            unsafe_failure.then_some(TerminationReason::UnsafeStop),
            ErrorCategory::SourceChanged,
            None,
        ));
    }
    let candidate_truncated = failures.len() > MAX_DIAGNOSTIC_EVENTS;
    failures.truncate(MAX_DIAGNOSTIC_EVENTS);
    let normalized = normalize_diagnostics(failures)
        .map_err(|error| ExecutorError::new(format!("cannot normalize diagnostics: {error}")))?;
    let failure_index_truncated = candidate_truncated || normalized.truncated;
    let failures = normalized.entries;
    let mut counts: BTreeMap<String, u32> = [
        "pending",
        "running",
        "reused",
        "passed",
        "failed",
        "timed_out",
        "invalidated",
        "not_meaningful",
        "cancelled",
        "unsafe",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), 0))
    .collect();
    for check in checks {
        let key = status_key(check.report.status).to_owned();
        *counts.entry(key).or_insert(0) += 1;
    }
    let phase_durations = aggregate_phase_times(checks.iter().flat_map(timing_nodes));
    let mut observed_capacity = capacity
        .lock()
        .map_err(|_| ExecutorError::new("capacity report lock poisoned"))?
        .clone();
    observed_capacity.capacity_wait_count = observed_capacity.capacity_wait_count.saturating_add(
        checks
            .iter()
            .filter_map(|check| check.report.execution.as_ref())
            .map(|execution| u64::from(execution.waiting))
            .sum::<u64>(),
    );
    let report = ExecutionReport {
        schema: Schema2,
        run_id: plan.run_id.clone(),
        test: plan.test.clone(),
        requested_tier: plan.requested_tier,
        readiness_eligible: plan.readiness_eligible,
        proof: plan.proof,
        selection: plan.selection.clone(),
        origin_run_id: plan.origin_run_id.clone(),
        status,
        started_at: started_at.to_owned(),
        finished_at,
        duration_seconds: seconds(started.elapsed()),
        source_digest: plan.source_digest.clone(),
        config_digest: plan.config_digest.clone(),
        source_changed,
        capacity: observed_capacity,
        counts,
        checks: checks
            .iter()
            .map(|check| {
                let mut report = check.report.clone();
                report.phase_durations = aggregate_phase_times(timing_nodes(check).into_iter());
                report
            })
            .collect(),
        phase_durations,
        failure_index: failures,
        failure_index_truncated,
    };
    write_json_atomic(path, &report)?;
    publish_observation(observations, &report);
    Ok(report)
}

fn timing_nodes(
    check: &CheckRuntime,
) -> Vec<(
    devcoordinator2_executor_protocol::CheckPhase,
    f64,
    Option<Instant>,
    Option<Instant>,
)> {
    use devcoordinator2_executor_protocol::CheckPhase;
    if check.processes.is_empty() {
        return vec![(
            check.plan.phase,
            // Receipt verification and queue time are not process execution.
            0.0,
            None,
            None,
        )];
    }
    check
        .processes
        .values()
        .map(|update| {
            let phase = match update.selector.phase {
                LogPhase::Fixture => CheckPhase::Fixture,
                LogPhase::Cleanup => CheckPhase::Cleanup,
                LogPhase::Case => CheckPhase::Case,
                LogPhase::Discovery => CheckPhase::Setup,
                _ => check.plan.phase,
            };
            (
                phase,
                update.progress.process_duration_ms as f64 / 1000.0,
                update.started_mono,
                update.finished_mono,
            )
        })
        .collect()
}

fn aggregate_phase_times(
    observations: impl Iterator<
        Item = (
            devcoordinator2_executor_protocol::CheckPhase,
            f64,
            Option<Instant>,
            Option<Instant>,
        ),
    >,
) -> Vec<PhaseDuration> {
    let mut phases = BTreeMap::<_, (f64, u32, Option<Instant>, Option<Instant>)>::new();
    for (phase, duration, start, end) in observations {
        let entry = phases.entry(phase).or_default();
        entry.0 += duration;
        entry.1 += 1;
        entry.2 = entry.2.into_iter().chain(start).min();
        entry.3 = entry.3.into_iter().chain(end).max();
    }
    phases
        .into_iter()
        .map(
            |(phase, (duration_seconds, checks, start, end))| PhaseDuration {
                phase,
                duration_seconds,
                checks,
                elapsed_seconds: start
                    .zip(end)
                    .map(|(start, end)| seconds(end.saturating_duration_since(start))),
            },
        )
        .collect()
}

fn publish_observation(sender: &watch::Sender<Option<ExecutionReport>>, report: &ExecutionReport) {
    if sender.receiver_count() > 0 {
        sender.send_replace(Some(report.clone()));
    }
}

fn observe_capacity(capacity: &Mutex<CapacityReport>, observation: CapacityObservation) {
    if let Ok(mut current) = capacity.lock() {
        if observation.learned_capacity.is_some() {
            current.learned_capacity = observation.learned_capacity;
        }
        if observation.effective_capacity.is_some() {
            current.effective_capacity = observation.effective_capacity;
        }
        if observation.waited {
            current.capacity_wait_count = current.capacity_wait_count.saturating_add(1);
        }
    }
}

fn check_index(checks: &[CheckRuntime], name: &str) -> Result<usize, ExecutorError> {
    checks
        .iter()
        .position(|check| check.plan.name == name)
        .ok_or_else(|| ExecutorError::new(format!("unknown active check {name:?}")))
}

fn bounded_reason(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if result.len() + character.len_utf8() > MAX_REASON_BYTES {
            break;
        }
        result.push(character);
    }
    result
}

fn diagnostic_exit(exit_code: Option<i32>) -> DiagnosticExit {
    match exit_code {
        Some(value) if value < 0 => DiagnosticExit {
            code: None,
            signal: u8::try_from(value.unsigned_abs()).ok(),
        },
        code => DiagnosticExit { code, signal: None },
    }
}

fn status_key(status: LeafStatus) -> &'static str {
    match status {
        LeafStatus::Pending => "pending",
        LeafStatus::Running => "running",
        LeafStatus::Reused => "reused",
        LeafStatus::Passed => "passed",
        LeafStatus::Failed => "failed",
        LeafStatus::TimedOut => "timed_out",
        LeafStatus::Invalidated => "invalidated",
        LeafStatus::NotMeaningful => "not_meaningful",
        LeafStatus::Cancelled => "cancelled",
        LeafStatus::Unsafe => "unsafe",
    }
}

fn iso_now() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso_at(since_epoch.as_secs())
}

fn iso_at(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn seconds(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1000.0).round() / 1000.0
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted.saturating_sub(146_096)
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn phase_times_separate_overlapping_execution_from_elapsed_time() {
        use super::*;
        use devcoordinator2_executor_protocol::CheckPhase;
        let start = Instant::now();
        let times = aggregate_phase_times(
            [
                (
                    CheckPhase::Build,
                    3.0,
                    Some(start),
                    Some(start + Duration::from_secs(3)),
                ),
                (
                    CheckPhase::Build,
                    3.0,
                    Some(start + Duration::from_secs(1)),
                    Some(start + Duration::from_secs(4)),
                ),
                (CheckPhase::Cleanup, 0.0, None, None),
            ]
            .into_iter(),
        );
        let build = times.iter().find(|p| p.phase == CheckPhase::Build).unwrap();
        assert_eq!(build.duration_seconds, 6.0);
        assert_eq!(build.elapsed_seconds, Some(4.0));
        assert_eq!(build.checks, 2);
        let cleanup = times
            .iter()
            .find(|p| p.phase == CheckPhase::Cleanup)
            .unwrap();
        assert_eq!(cleanup.elapsed_seconds, None);
    }

    #[test]
    fn civil_date_conversion_matches_epoch_and_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
