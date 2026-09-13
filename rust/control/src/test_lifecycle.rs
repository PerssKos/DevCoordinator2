//! Governed-test lifecycle orchestration around the non-root Rust executor.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use devcoordinator2_api::params::{RetryTest, StartTest};
use devcoordinator2_api::results::{
    ArtifactReceipt as ApiArtifactReceipt, CaseProjection, CheckProjection,
    DiagnosticExit as ApiDiagnosticExit, ExecutionCapacity, ExecutionLogStreamSummary,
    ProofKind as ApiProofKind, StopTest, TestList, TestListRow, TestStarted, TestStatus,
    TestSummary,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use devcoordinator2_executor_protocol::{
    CheckPlan, CheckReport, ExecutionPlan, ExecutionReport, LeafStatus, ProofKind, RunStatus,
    Schema2, ValidationTier,
};
use rusqlite::OptionalExtension;
use time::{format_description::FormatItem, macros::format_description};

use crate::access::Caller;
use crate::capacity::{CapacityBroker, HostMemory};
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::docker::{
    DockerCli, DockerControl, ExactContainerId, ManagedLabelContext, RunDetachedRequest,
};
use crate::metrics_source::{HostMetricSource, MetricSource};
use crate::platform::{Clock, HostMonotonicClock, HostRandom, MonotonicClock, RandomSource};
use crate::repository::{Registry, resolve_worktree};
use crate::repository_config::{TestSpec, load_test_spec};
use crate::systemd::{SystemdCli, SystemdControl, TransientUnitSpec, UnitProcess};
use crate::test_admission::{AdmissionError, TestAdmission};
use crate::test_command::{HostTestCommand, TestCommand};
use crate::test_logs::TestLogService;
use crate::test_state::{
    ENV_FILE, PLAN_FILE, PreparedRun, RetryEvidence, TestRunStore, api_tier, executor_tier,
    initial_summary, terminal_status,
};

const CHECK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const LAUNCH_VERIFY: Duration = Duration::from_secs(10);
const STOP_WAIT: Duration = Duration::from_secs(10);
const LOOP_WAKE: Duration = Duration::from_millis(50);
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
const RUN_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

#[derive(Clone)]
pub struct TestLifecycle {
    inner: Arc<Inner>,
}

#[derive(Clone, Debug)]
pub struct TestLifecycleEvent {
    pub kind: &'static str,
    pub run_id: String,
    pub test: String,
    pub status: Option<TestStatus>,
    pub exit_code: Option<i32>,
    pub repository_id: String,
    pub worktree_id: String,
    pub duration_seconds: Option<f64>,
    pub caller_uid: u32,
    pub client: String,
}

pub trait TestEventSink: Send + Sync + 'static {
    fn publish(&self, event: TestLifecycleEvent);
}

impl<F> TestEventSink for F
where
    F: Fn(TestLifecycleEvent) + Send + Sync + 'static,
{
    fn publish(&self, event: TestLifecycleEvent) {
        self(event);
    }
}

struct Inner {
    config: Arc<Config>,
    database: Database,
    registry: Registry,
    admission: TestAdmission,
    capacity: CapacityBroker,
    logs: TestLogService,
    systemd: Arc<dyn SystemdControl>,
    docker: Arc<dyn DockerControl>,
    command: Arc<dyn TestCommand>,
    store: TestRunStore,
    clock: Arc<dyn Clock>,
    monotonic: Arc<dyn MonotonicClock>,
    random: Arc<dyn RandomSource>,
    executor: PathBuf,
    events: Mutex<Option<Arc<dyn TestEventSink>>>,
    runs: Mutex<HashMap<String, Arc<RunHandle>>>,
}

struct RunHandle {
    work: Option<devcoordinator2_api::work_context::WorkAttribution>,
    run_id: String,
    test: String,
    unit: String,
    worktree_id: String,
    worktree: PathBuf,
    repository_id: String,
    caller_uid: u32,
    caller_gid: u32,
    client: String,
    started_at: String,
    started_mono: f64,
    proof: ProofKind,
    selection: Vec<String>,
    origin_run_id: Option<String>,
    requested_tier: ValidationTier,
    current: File,
    stdout_bytes: Arc<AtomicU64>,
    stderr_bytes: Arc<AtomicU64>,
    containers: Mutex<Vec<ExactContainerId>>,
    state: Mutex<RunState>,
    finalized: Condvar,
}

#[derive(Default)]
struct RunState {
    stop: Option<RequestedStop>,
    final_status: Option<TestStatus>,
    cleanup_complete: bool,
    evidence_complete: bool,
}

struct RequestedStop {
    status: TestStatus,
    detail: Option<String>,
    termination: Option<devcoordinator2_api::results::RunTerminationReason>,
}

struct Drain {
    join: Option<thread::JoinHandle<io::Result<()>>>,
    failed: Arc<AtomicBool>,
}

impl Drain {
    fn start(
        source: Option<Box<dyn Read + Send>>,
        destination: File,
        count: Arc<AtomicU64>,
    ) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let failed_reader = Arc::clone(&failed);
        let join = source.map(|mut source| {
            thread::spawn(move || {
                let mut destination = destination;
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    let read = match source.read(&mut buffer) {
                        Ok(read) => read,
                        Err(error) => {
                            failed_reader.store(true, Ordering::SeqCst);
                            return Err(error);
                        }
                    };
                    if read == 0 {
                        destination.sync_all()?;
                        return Ok(());
                    }
                    count.fetch_add(u64::try_from(read).unwrap_or(u64::MAX), Ordering::SeqCst);
                    if let Err(error) = destination.write_all(&buffer[..read]) {
                        failed_reader.store(true, Ordering::SeqCst);
                        return Err(error);
                    }
                }
            })
        });
        Self { join, failed }
    }

    fn finish(mut self) -> bool {
        let joined = self
            .join
            .take()
            .is_none_or(|join| join.join().is_ok_and(|result| result.is_ok()));
        joined && !self.failed.load(Ordering::SeqCst)
    }

    fn failure_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.failed)
    }
}

enum LaunchState {
    Active,
    Exited,
}

struct EphemeralPostgres {
    container: ExactContainerId,
    environment: BTreeMap<String, String>,
}

impl TestLifecycle {
    pub fn new(
        config: Config,
        database: Database,
        registry: Registry,
        capacity: CapacityBroker,
        logs: TestLogService,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProtocolError> {
        Self::with_adapters(
            config,
            database,
            registry,
            capacity,
            logs,
            Arc::new(SystemdCli::default()),
            Arc::new(DockerCli::default()),
            Arc::new(HostTestCommand),
            TestRunStore,
            clock,
            Arc::new(HostMonotonicClock::default()),
            Arc::new(HostRandom),
            default_executor_path(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_adapters(
        config: Config,
        database: Database,
        registry: Registry,
        capacity: CapacityBroker,
        logs: TestLogService,
        systemd: Arc<dyn SystemdControl>,
        docker: Arc<dyn DockerControl>,
        command: Arc<dyn TestCommand>,
        store: TestRunStore,
        clock: Arc<dyn Clock>,
        monotonic: Arc<dyn MonotonicClock>,
        random: Arc<dyn RandomSource>,
        executor: PathBuf,
    ) -> Result<Self, ProtocolError> {
        let runtime = config.socket_path.parent().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "daemon socket has no runtime directory",
            )
        })?;
        let admission = TestAdmission::new(runtime).map_err(admission_error)?;
        let lifecycle = Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                database,
                registry,
                admission,
                capacity,
                logs,
                systemd,
                docker,
                command,
                store,
                clock,
                monotonic,
                random,
                executor,
                events: Mutex::new(None),
                runs: Mutex::new(HashMap::new()),
            }),
        };
        let weak = Arc::downgrade(&lifecycle.inner);
        lifecycle.inner.capacity.set_memory_pressure_handler(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                let lifecycle = Self { inner };
                if let Err(error) = lifecycle.relieve_memory_pressure(|| HostMemory::read(Path::new("/proc"))) {
                    tracing::error!(code = ?error.code, "governed memory emergency cleanup failed");
                }
            }
        }));
        Ok(lifecycle)
    }

    pub fn set_event_sink(&self, sink: Arc<dyn TestEventSink>) {
        *self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    pub fn start(&self, params: StartTest, caller: &Caller) -> Result<TestStarted, ProtocolError> {
        self.start_inner(
            &params.path,
            params.test.as_deref(),
            &params.checks,
            executor_tier(params.tier),
            None,
            caller,
        )
    }

    pub fn retry(&self, params: RetryTest, caller: &Caller) -> Result<TestStarted, ProtocolError> {
        self.start_inner(
            &params.path,
            params.test.as_deref(),
            std::slice::from_ref(&params.check),
            ValidationTier::Release,
            Some((&params.run_id, &params.check)),
            caller,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn start_inner(
        &self,
        path: &str,
        test_name: Option<&str>,
        requested: &[String],
        mut requested_tier: ValidationTier,
        retry: Option<(&str, &str)>,
        caller: &Caller,
    ) -> Result<TestStarted, ProtocolError> {
        if caller.uid == 0 {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "repository code never runs as root; call as a non-root account",
            ));
        }
        if retry.is_some() && requested.len() != 1 {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "retry and explicit check selection are separate modes",
            ));
        }
        let registered = self
            .inner
            .registry
            .register(Path::new(path), caller.uid, caller.gid)?;
        let worktree = PathBuf::from(&registered.worktree_path);
        let specification = load_test_spec(&worktree, test_name).map_err(config_error)?;
        if !self.inner.command.executor_available(&self.inner.executor) {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "Rust governed-test executor is unavailable; build the locked release target before starting tests",
            ));
        }
        let source_digest = self
            .inner
            .command
            .source_digest(&self.inner.executor, &worktree, caller.uid, caller.gid)
            .map_err(command_start_error)?;
        let origin = if let Some((run_id, check)) = retry {
            let mut origin = self
                .inner
                .store
                .find_evidence(&worktree, run_id)
                .map_err(state_start_error)?;
            if origin.is_none()
                && let Some(summary) = self
                    .inner
                    .store
                    .read_current_summary(&worktree)
                    .map_err(state_start_error)?
                && summary.run_id == run_id
                && summary.status != TestStatus::Running
                && let Some(current) = self
                    .inner
                    .store
                    .open_current(&worktree)
                    .map_err(state_start_error)?
                && let Some(report) = self.inner.store.read_report(&current).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::TestStartFailed,
                        "retained check report cannot establish retry evidence",
                    )
                })?
                && report.run_id == run_id
                && report.test == summary.test
                && api_proof(report.proof) == summary.proof
                && report.selection == summary.selection
                && report.origin_run_id == summary.origin_run_id
                && api_tier(report.requested_tier) == summary.requested_tier
            {
                self.inner
                    .store
                    .record_evidence(
                        &worktree,
                        &report,
                        &summary.status,
                        summary.work.as_ref(),
                        caller.uid,
                        caller.gid,
                    )
                    .map_err(state_start_error)?;
                origin = self
                    .inner
                    .store
                    .find_evidence(&worktree, run_id)
                    .map_err(state_start_error)?;
            }
            let origin = origin.ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::TestStartFailed,
                    format!("no completed evidence for run {run_id}"),
                )
            })?;
            validate_retry(&origin, run_id, check, &specification, &source_digest)?;
            requested_tier = origin.requested_tier;
            Some(origin)
        } else {
            None
        };
        let selected = selected_closure(&specification.checks, requested, requested_tier)?;
        self.validate_executables(&worktree, &specification, &selected, caller.uid, caller.gid)?;

        let mut admission = self
            .inner
            .admission
            .start_guard_for(&registered.worktree_id)
            .map_err(admission_error)?;
        let superseded_run_id = self.supersede_prior(&registered.worktree_id)?;
        let run_id = self.run_id()?;
        let prepared = self
            .inner
            .store
            .prepare(&worktree, &run_id, caller.uid, caller.gid)
            .map_err(state_start_error)?;
        let proof = if retry.is_some() {
            ProofKind::Retry
        } else if requested.is_empty() {
            ProofKind::Complete
        } else {
            ProofKind::Selected
        };
        let started_at = self.timestamp()?;
        let client = client_name(caller);
        let mut summary = initial_summary(
            &run_id,
            &specification.name,
            &started_at,
            caller.uid,
            &client,
            proof,
            requested.to_vec(),
            retry.map(|value| value.0.to_owned()),
            requested_tier,
        );
        summary.work = caller.work.clone();
        if let Err(error) =
            self.inner
                .store
                .write_summary(&prepared.current, &summary, caller.uid, caller.gid)
        {
            self.cleanup_unstarted(&worktree, &run_id);
            return Err(state_start_error(error));
        }
        let unit = crate::ids::unit_name(
            &self.inner.config.unit_prefix,
            &registered.worktree_id,
            run_id.trim_start_matches('t'),
        );
        if let Err(error) = admission.started(&run_id, &unit) {
            self.cleanup_unstarted(&worktree, &run_id);
            return Err(admission_error(error));
        }
        drop(admission);
        if let Err(error) = self.inner.capacity.register_run(&run_id, caller.uid) {
            let _ = self.inner.admission.finished(&run_id);
            self.cleanup_unstarted(&worktree, &run_id);
            return Err(error);
        }

        let mut environment = specification.env.clone();
        environment
            .entry("PATH".into())
            .or_insert_with(|| CHECK_PATH.into());
        environment.insert(
            "DEVCOORDINATOR_CAPACITY_SOCKET".into(),
            self.inner
                .capacity
                .socket_path()
                .to_string_lossy()
                .into_owned(),
        );
        let mut containers = Vec::new();
        if let Some(postgres) = &specification.postgres {
            match self.provision_postgres(
                postgres,
                &run_id,
                &registered.repository_id,
                &registered.worktree_id,
                &started_at,
                caller,
            ) {
                Ok(postgres) => {
                    environment.extend(postgres.environment);
                    containers.push(postgres.container);
                    let identities = containers
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>();
                    if let Err(error) = self.inner.store.write_containers(
                        &prepared.current,
                        &identities,
                        caller.uid,
                        caller.gid,
                    ) {
                        self.rollback_accepted_start(&worktree, &run_id, &containers);
                        return Err(state_start_error(error));
                    }
                }
                Err(error) => {
                    self.rollback_accepted_start(&worktree, &run_id, &containers);
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.inner.store.write_environment(
            &prepared.current,
            &environment,
            caller.uid,
            caller.gid,
        ) {
            self.rollback_accepted_start(&worktree, &run_id, &containers);
            return Err(state_start_error(error));
        }
        let plan = match self.build_plan(
            &specification,
            &selected,
            &run_id,
            &worktree,
            &prepared,
            &source_digest,
            requested,
            origin.as_ref(),
            retry.map(|value| value.0),
            requested_tier,
            caller,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.rollback_accepted_start(&worktree, &run_id, &containers);
                return Err(error);
            }
        };
        if let Err(error) =
            self.inner
                .store
                .write_plan(&prepared.current, &plan, caller.uid, caller.gid)
        {
            self.rollback_accepted_start(&worktree, &run_id, &containers);
            return Err(state_start_error(error));
        }
        let unit_specification = TransientUnitSpec {
            unit: unit.clone(),
            slice_name: self.inner.config.slice_name.clone(),
            uid: caller.uid,
            gid: caller.gid,
            timeout_seconds: specification.timeout_seconds,
            working_directory: worktree.clone(),
            environment_file: Some(prepared.current_path.join(ENV_FILE)),
            command: vec![
                self.inner.executor.as_os_str().to_owned(),
                OsString::from("run"),
                prepared.current_path.join(PLAN_FILE).into_os_string(),
            ],
            scratch_directory: prepared.current_path.join("scratch"),
        };
        let mut process = match self.inner.systemd.spawn_transient(&unit_specification) {
            Ok(process) => process,
            Err(error) => {
                self.rollback_accepted_start(&worktree, &run_id, &containers);
                return Err(ProtocolError::new(
                    ErrorCode::TestStartFailed,
                    format!("cannot spawn systemd-run: {error}"),
                ));
            }
        };
        let stdout_bytes = Arc::new(AtomicU64::new(0));
        let stderr_bytes = Arc::new(AtomicU64::new(0));
        let stdout = Drain::start(
            process.take_stdout(),
            prepared.executor_stdout,
            Arc::clone(&stdout_bytes),
        );
        let stderr = Drain::start(
            process.take_stderr(),
            prepared.executor_stderr,
            Arc::clone(&stderr_bytes),
        );
        let handle = Arc::new(RunHandle {
            work: caller.work.clone(),
            run_id: run_id.clone(),
            test: specification.name.clone(),
            unit: unit.clone(),
            worktree_id: registered.worktree_id.clone(),
            worktree: worktree.clone(),
            repository_id: registered.repository_id.clone(),
            caller_uid: caller.uid,
            caller_gid: caller.gid,
            client,
            started_at,
            started_mono: self.inner.monotonic.seconds(),
            proof,
            selection: requested.to_vec(),
            origin_run_id: retry.map(|value| value.0.to_owned()),
            requested_tier,
            current: prepared.current,
            stdout_bytes,
            stderr_bytes,
            containers: Mutex::new(containers),
            state: Mutex::new(RunState::default()),
            finalized: Condvar::new(),
        });
        let launch = self.verify_launch(
            &handle,
            process.as_mut(),
            &[stdout.failure_flag(), stderr.failure_flag()],
        );
        match launch {
            Ok(LaunchState::Active | LaunchState::Exited) => {
                self.inner
                    .runs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(registered.worktree_id.clone(), Arc::clone(&handle));
                self.spawn_reaper(handle, process, stdout, stderr);
                let started = TestStarted {
                    run_id,
                    repository_id: registered.repository_id,
                    worktree_id: registered.worktree_id,
                    test: specification.name,
                    status: TestStatus::Running,
                    proof: api_proof(proof),
                    selection: requested.to_vec(),
                    origin_run_id: retry.map(|value| value.0.to_owned()),
                    requested_tier: api_tier(requested_tier),
                    readiness_eligible: proof == ProofKind::Complete
                        && requested_tier == ValidationTier::Release,
                    superseded_run_id,
                    unit,
                    summary_ref: "summary.json".into(),
                };
                self.publish_event(TestLifecycleEvent {
                    kind: "test.started",
                    run_id: started.run_id.clone(),
                    test: started.test.clone(),
                    status: Some(TestStatus::Running),
                    exit_code: None,
                    repository_id: started.repository_id.clone(),
                    worktree_id: started.worktree_id.clone(),
                    duration_seconds: None,
                    caller_uid: caller.uid,
                    client: client_name(caller),
                });
                Ok(started)
            }
            Err(error) => {
                let _ = self.stop_unit(&unit);
                let exit = process.wait().ok();
                self.finalize(&handle, exit, stdout.finish() && stderr.finish());
                Err(error)
            }
        }
    }

    pub fn status(&self, path: &str, caller: &Caller) -> Result<TestSummary, ProtocolError> {
        let (worktree, worktree_id) = self.resolve(path, caller)?;
        let mut summary = self
            .inner
            .store
            .read_current_summary(&worktree)
            .map_err(state_error)?
            .ok_or_else(test_not_found)?;
        if let Some(handle) = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&worktree_id)
            .cloned()
            && summary.status == TestStatus::Running
        {
            self.project_live(&mut summary, &handle)?;
        }
        summary.capacity = Some(self.inner.capacity.snapshot()?);
        reconcile_terminal_checks(&mut summary);
        bound_summary_response(&mut summary, 128 * 1024)?;
        Ok(summary)
    }

    pub fn history(
        &self,
        params: devcoordinator2_api::params::TestHistory,
        caller: &Caller,
    ) -> Result<devcoordinator2_api::results::TestHistory, ProtocolError> {
        if !(1..=50).contains(&params.limit) {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "Run history limit must be between 1 and 50.",
            ));
        }
        let worktree = self.resolve_history_worktree(&params.path, caller)?;
        let mut records = self
            .inner
            .store
            .read_history(&worktree)
            .map_err(state_error)?;
        if let Some(current) = self
            .inner
            .store
            .read_current_summary(&worktree)
            .map_err(state_error)?
        {
            records.retain(|record| record.run_id != current.run_id);
            records.push(crate::test_state::TestHistoryEntry {
                work: current.work,
                run_id: current.run_id,
                test: current.test,
                status: current.status,
                started_at: current.started_at,
                finished_at: current.finished_at,
                duration_seconds: current.duration_seconds,
                exit_code: current.exit_code,
                termination_reason: current.termination_reason,
            });
        }
        records.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| right.run_id.cmp(&left.run_id))
        });
        let start = match params.before {
            Some(before) => records
                .iter()
                .position(|record| record.run_id == before)
                .map(|position| position + 1)
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "Run history changed. Reload its newest page.",
                    )
                })?,
            None => 0,
        };
        let end = (start + usize::from(params.limit)).min(records.len());
        let mut runs = Vec::new();
        let mut bytes = 0;
        for record in &records[start..end] {
            let row = devcoordinator2_api::results::TestHistoryRun {
                work: record.work.clone(),
                run_id: record.run_id.clone(),
                test: record.test.clone(),
                status: record.status.clone(),
                started_at: record.started_at.clone(),
                finished_at: record.finished_at.clone(),
                duration_seconds: record.duration_seconds,
                termination_reason: record.termination_reason.clone(),
            };
            let size = serde_json::to_vec(&row)
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::InternalError, "Cannot encode run history")
                })?
                .len();
            if size > 12_288 {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "Run history entry exceeds the bounded page size",
                ));
            }
            if bytes + size > 12_288 {
                break;
            }
            bytes += size;
            runs.push(row);
        }
        let next_before = (start + runs.len() < records.len())
            .then(|| runs.last().expect("nonempty history page").run_id.clone());
        Ok(devcoordinator2_api::results::TestHistory { runs, next_before })
    }

    pub fn current_summary_ref(
        &self,
        path: &str,
        caller: &Caller,
    ) -> Option<devcoordinator2_api::results::CurrentTest> {
        let (worktree, _) = self.resolve(path, caller).ok()?;
        let summary = self.inner.store.read_current_summary(&worktree).ok()??;
        Some(devcoordinator2_api::results::CurrentTest {
            status: summary.status,
            run_id: summary.run_id,
        })
    }

    pub fn stop(
        &self,
        path: &str,
        reason: Option<String>,
        caller: &Caller,
    ) -> Result<StopTest, ProtocolError> {
        let (worktree, worktree_id) = self.resolve(path, caller)?;
        let summary = self
            .inner
            .store
            .read_current_summary(&worktree)
            .map_err(state_error)?
            .ok_or_else(test_not_found)?;
        let handle = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&worktree_id)
            .cloned();
        let Some(handle) = handle else {
            return Ok(StopTest::AlreadyFinished {
                run_id: summary.run_id,
                status: summary.status,
                already_finished: true,
            });
        };
        {
            let mut state = handle
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(status) = &state.final_status {
                return Ok(StopTest::AlreadyFinished {
                    run_id: handle.run_id.clone(),
                    status: status.clone(),
                    already_finished: true,
                });
            }
            state.stop = Some(RequestedStop {
                status: TestStatus::Cancelled,
                detail: reason,
                termination: None,
            });
        }
        self.stop_unit(&handle.unit)?;
        let state = handle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = handle
            .finalized
            .wait_timeout_while(state, STOP_WAIT, |state| !state.cleanup_complete)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = state.final_status.clone().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::UnitStopFailed,
                "test unit stopped but finalization did not complete",
            )
        })?;
        Ok(StopTest::Cancelled {
            run_id: handle.run_id.clone(),
            status,
        })
    }

    /// Called by the capacity sampler's single emergency worker. Only exact,
    /// currently owned test handles are eligible; deployment and foreign units
    /// never enter this list. Re-sample after cleanup instead of cancelling a
    /// whole batch based on stale host pressure.
    pub fn relieve_memory_pressure(
        &self,
        mut memory: impl FnMut() -> Option<HostMemory>,
    ) -> Result<Vec<String>, ProtocolError> {
        if !memory().is_some_and(HostMemory::emergency) {
            return Ok(Vec::new());
        }
        let handles = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let source = HostMetricSource::new(Arc::clone(&self.inner.docker));
        let mut candidates = handles
            .into_iter()
            .filter_map(|handle| {
                let state = handle
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.final_status.is_some()
                    || state.stop.as_ref().is_some_and(|stop| {
                        stop.termination.as_ref()
                            != Some(
                                &devcoordinator2_api::results::RunTerminationReason::MemoryPressure,
                            )
                    })
                {
                    return None;
                }
                drop(state);
                let properties = self
                    .inner
                    .systemd
                    .show_unit(&handle.unit, &["ActiveState", "MemoryCurrent"])
                    .ok()?;
                if property(&properties, "ActiveState") != Some("active") {
                    return None;
                }
                let mut bytes = property(&properties, "MemoryCurrent")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                for container in handle
                    .containers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                {
                    let cgroup = source.container_cgroup(container.as_str());
                    bytes = bytes.saturating_add(
                        self.inner
                            .systemd
                            .cgroup_memory_current(&cgroup)
                            .unwrap_or(0),
                    );
                }
                (bytes > 0).then_some((bytes, handle))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.run_id.cmp(&right.1.run_id))
        });
        let mut stopped = Vec::new();
        for (index, (_, handle)) in candidates.iter().enumerate() {
            let Some(host) = memory().filter(|host| host.emergency()) else {
                break;
            };
            // Do not kill negligible test wrappers when pressure belongs to
            // unrelated services. Several contributing tests count together.
            let remaining_bytes = candidates[index..]
                .iter()
                .fold(0_u64, |sum, (bytes, _)| sum.saturating_add(*bytes));
            if remaining_bytes < host.total / 100 {
                break;
            }
            {
                let mut state = handle
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.final_status.is_some()
                    || state.stop.as_ref().is_some_and(|stop| {
                        stop.termination.as_ref()
                            != Some(
                                &devcoordinator2_api::results::RunTerminationReason::MemoryPressure,
                            )
                    })
                {
                    continue;
                }
                state.stop = Some(RequestedStop {
                    status: TestStatus::Failed,
                    detail: None,
                    termination: Some(
                        devcoordinator2_api::results::RunTerminationReason::MemoryPressure,
                    ),
                });
            }
            self.stop_unit(&handle.unit)?;
            let state = handle
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (state, _) = handle
                .finalized
                .wait_timeout_while(state, STOP_WAIT, |state| !state.cleanup_complete)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.cleanup_complete || state.final_status.is_none() {
                return Err(ProtocolError::new(
                    ErrorCode::UnitStopFailed,
                    "memory emergency test cleanup did not complete",
                ));
            }
            if !state.evidence_complete {
                return Err(ProtocolError::new(
                    ErrorCode::UnitStopFailed,
                    "memory emergency test stopped but its evidence could not be retained",
                ));
            }
            stopped.push(handle.run_id.clone());
        }
        Ok(stopped)
    }

    pub fn list_current(&self) -> Result<TestList, ProtocolError> {
        self.list_current_page(devcoordinator2_api::params::TestList::default())
    }

    pub fn list_current_page(
        &self,
        params: devcoordinator2_api::params::TestList,
    ) -> Result<TestList, ProtocolError> {
        let limit = usize::from(params.limit.unwrap_or(50));
        if !(1..=50).contains(&limit)
            || params
                .after_worktree_id
                .as_ref()
                .is_some_and(|id| id.len() > 128)
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "test list requires limit 1..50 and a bounded worktree cursor",
            ));
        }
        let rows = self
            .inner
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT w.worktree_id,w.worktree_path,w.repository_id,r.display_name FROM worktrees w JOIN repositories r ON r.repository_id=w.repository_id WHERE r.archived_at IS NULL AND w.worktree_id > ?1 ORDER BY w.worktree_id",
                )?;
                Ok(statement
                    .query_map([params.after_worktree_id.as_deref().unwrap_or("")], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)?;
        let capacity = self.inner.capacity.snapshot()?;
        let active = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut runs = Vec::new();
        let mut sources = HashMap::new();
        let mut bytes = 0usize;
        let mut next_worktree_id = None;
        for (worktree_id, path, repository_id, display_name) in rows {
            let worktree = PathBuf::from(&path);
            let Some(mut summary) = self
                .inner
                .store
                .read_current_summary(&worktree)
                .map_err(state_error)?
            else {
                continue;
            };
            if let Some(handle) = active.get(&worktree_id)
                && summary.status == TestStatus::Running
            {
                self.project_live(&mut summary, handle)?;
            }
            if runs.len() >= limit {
                next_worktree_id = runs.last().map(|run: &TestListRow| run.worktree_id.clone());
                break;
            }
            summary.capacity = Some(capacity.clone());
            reconcile_terminal_checks(&mut summary);
            compact_list_summary(&mut summary);
            bound_summary_response(&mut summary, 48 * 1024)?;
            let repository_source = sources
                .entry(repository_id.clone())
                .or_insert_with(|| {
                    let metadata = worktree.metadata().ok()?;
                    crate::repository::test_repository_source(
                        &worktree,
                        (summary.caller_uid, metadata.gid()),
                    )
                })
                .clone();
            let row = TestListRow {
                worktree_id,
                worktree_path: path,
                repository_id,
                display_name,
                repository_source,
                earlier_visual_evidence: None,
                visual_evidence: devcoordinator2_api::results::VisualEvidenceSummary {
                    status: "unavailable".to_owned(),
                    bundle_count: 0,
                    image_count: 0,
                    issue_count: 0,
                    issues_truncated: false,
                    error_code: None,
                },
                summary,
            };
            let row_bytes = serde_json::to_vec(&row)
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::InternalError, "test row encoding failed")
                })?
                .len();
            if !runs.is_empty() && bytes.saturating_add(row_bytes) > 160 * 1024 {
                next_worktree_id = runs.last().map(|run| run.worktree_id.clone());
                break;
            }
            bytes += row_bytes;
            runs.push(row);
        }
        Ok(TestList {
            runs,
            next_worktree_id,
        })
    }

    pub fn recover(&self) -> Result<(), ProtocolError> {
        let pattern = format!("{}-*.service", self.inner.config.unit_prefix);
        for unit in self
            .inner
            .systemd
            .list_matching_units(&pattern)
            .map_err(systemd_error)?
        {
            let _ = self.stop_unit(&unit);
        }
        let labels = BTreeMap::from([
            (
                "devcoordinator2.instance".into(),
                self.inner.config.unit_prefix.clone(),
            ),
            ("devcoordinator2.purpose".into(), "test".into()),
        ]);
        if let Ok(containers) = self.inner.docker.list_ids_by_labels(&labels) {
            self.remove_containers(&containers);
        }
        for worktree in self.inner.registry.registered_worktree_paths()? {
            let Some(mut summary) = self
                .inner
                .store
                .read_current_summary(&worktree)
                .map_err(state_error)?
            else {
                continue;
            };
            if summary.status != TestStatus::Running {
                continue;
            }
            let Some(current) = self
                .inner
                .store
                .open_current(&worktree)
                .map_err(state_error)?
            else {
                continue;
            };
            terminal_status(
                &mut summary,
                TestStatus::Interrupted,
                self.timestamp()?,
                0.0,
                None,
                Some(devcoordinator2_api::results::RunTerminationReason::Interrupted),
            );
            let (owner_uid, owner_gid) = self.inner.store.owner(&current).map_err(state_error)?;
            let _ = self
                .inner
                .store
                .write_summary(&current, &summary, owner_uid, owner_gid);
            let _ = self
                .inner
                .store
                .record_history(&worktree, &summary, owner_uid, owner_gid);
        }
        self.inner.admission.reset().map_err(admission_error)
    }

    #[allow(clippy::too_many_arguments)]
    fn provision_postgres(
        &self,
        specification: &crate::repository_config::PostgresSpec,
        run_id: &str,
        repository_id: &str,
        worktree_id: &str,
        started_at: &str,
        caller: &Caller,
    ) -> Result<EphemeralPostgres, ProtocolError> {
        if !self.inner.docker.available() {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "ephemeral PostgreSQL failed: Docker is unavailable",
            ));
        }
        if specification.image.contains("@sha256:") {
            self.inner.docker.ensure_digest_image(&specification.image)
        } else {
            self.inner.docker.ensure_image(&specification.image)
        }
        .map_err(postgres_error)?;
        let mut random = [0_u8; 24];
        self.inner.random.fill(&mut random).map_err(|error| {
            ProtocolError::new(
                ErrorCode::TestStartFailed,
                "ephemeral PostgreSQL credentials could not be generated",
            )
            .with_detail(error.to_string())
        })?;
        let password = base64_url_no_pad(&random);
        let values = BTreeMap::from([
            ("POSTGRES_USER".into(), specification.user.clone()),
            ("POSTGRES_PASSWORD".into(), password.clone()),
            ("POSTGRES_DB".into(), specification.database.clone()),
            ("PGDATA".into(), "/var/lib/postgresql/data".into()),
        ]);
        let container = self
            .inner
            .docker
            .run_detached(&RunDetachedRequest {
                name: format!(
                    "devcoordinator2-test-{}-postgres",
                    run_id.trim_start_matches('t')
                ),
                image: specification.image.clone(),
                label_context: ManagedLabelContext {
                    instance: self.inner.config.unit_prefix.clone(),
                    repository_id: repository_id.into(),
                    worktree_id: worktree_id.into(),
                    run_id: Some(run_id.into()),
                    deployment_id: None,
                    component: None,
                    generation: None,
                    ttl_seconds: None,
                    purpose: "test".into(),
                    caller_uid: caller.uid,
                    client: client_name(caller),
                    session: caller.client_session.clone(),
                    created_at: started_at.into(),
                    data_class: "disposable".into(),
                },
                labels: BTreeMap::new(),
                env_names: vec![
                    "POSTGRES_USER".into(),
                    "POSTGRES_PASSWORD".into(),
                    "POSTGRES_DB".into(),
                    "PGDATA".into(),
                ],
                env_values: values,
                publish: vec!["127.0.0.1::5432".into()],
                tmpfs: vec!["/var/lib/postgresql/data:rw,size=1g,mode=0700".into()],
                command: Vec::new(),
            })
            .map_err(postgres_error)?;
        let prepared = (|| {
            let port = self
                .inner
                .docker
                .published_host_port(&container, "5432/tcp")?;
            self.inner.docker.wait_postgres_ready(
                &container,
                &specification.user,
                &specification.database,
                Duration::from_secs(90),
            )?;
            Ok::<_, crate::docker::DockerError>(port)
        })();
        let port = match prepared {
            Ok(port) => port,
            Err(error) => {
                let _ = self.inner.docker.remove_container(&container, true);
                return Err(postgres_error(error));
            }
        };
        let url = format!(
            "postgresql://{}:{}@127.0.0.1:{}/{}",
            specification.user, password, port, specification.database
        );
        Ok(EphemeralPostgres {
            container,
            environment: BTreeMap::from([
                ("PGHOST".into(), "127.0.0.1".into()),
                ("PGPORT".into(), port.to_string()),
                ("PGUSER".into(), specification.user.clone()),
                ("PGPASSWORD".into(), password),
                ("PGDATABASE".into(), specification.database.clone()),
                ("DATABASE_URL".into(), url),
            ]),
        })
    }

    fn resolve_history_worktree(
        &self,
        path: &str,
        caller: &Caller,
    ) -> Result<PathBuf, ProtocolError> {
        if !Path::new(path).is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be absolute",
            ));
        }
        if caller.identity.is_none() {
            return self.resolve(path, caller).map(|(worktree, _)| worktree);
        }
        let requested = path.to_owned();
        self.inner
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT worktree_path FROM worktrees WHERE worktree_path=?1",
                        [&requested],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .map(PathBuf::from)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    "No registered worktree matches this request.",
                )
            })
    }

    fn resolve(&self, path: &str, caller: &Caller) -> Result<(PathBuf, String), ProtocolError> {
        let info = resolve_worktree(Path::new(path), Some((caller.uid, caller.gid)))?;
        let worktree_id = crate::ids::worktree_id(&info.worktree_root).map_err(|error| {
            ProtocolError::new(ErrorCode::RepositoryNotFound, "cannot identify worktree")
                .with_detail(error.to_string())
        })?;
        Ok((info.worktree_root, worktree_id))
    }

    fn validate_executables(
        &self,
        worktree: &Path,
        specification: &TestSpec,
        selected: &[CheckPlan],
        uid: u32,
        gid: u32,
    ) -> Result<(), ProtocolError> {
        for check in selected {
            for command in [&check.command, &check.discover, &check.case_command]
                .into_iter()
                .flatten()
            {
                let executable = &command[0];
                let path = check
                    .env
                    .get("PATH")
                    .or_else(|| specification.env.get("PATH"))
                    .map_or(CHECK_PATH, String::as_str);
                let cwd = if check.cwd == "." {
                    worktree.to_owned()
                } else {
                    worktree.join(&check.cwd)
                };
                let candidates = if executable.contains('/') {
                    vec![if Path::new(executable).is_absolute() {
                        PathBuf::from(executable)
                    } else {
                        cwd.join(executable)
                    }]
                } else {
                    path.split(':')
                        .filter(|part| !part.is_empty())
                        .map(|part| Path::new(part).join(executable))
                        .collect()
                };
                if !candidates
                    .iter()
                    .any(|candidate| self.inner.command.executable(candidate, uid, gid))
                {
                    return Err(ProtocolError::new(
                        ErrorCode::TestStartFailed,
                        format!(
                            "check {:?} executable {:?} is unavailable",
                            check.name, executable
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_plan(
        &self,
        specification: &TestSpec,
        selected: &[CheckPlan],
        run_id: &str,
        worktree: &Path,
        prepared: &PreparedRun,
        source_digest: &str,
        requested: &[String],
        origin: Option<&RetryEvidence>,
        origin_run_id: Option<&str>,
        requested_tier: ValidationTier,
        caller: &Caller,
    ) -> Result<ExecutionPlan, ProtocolError> {
        let selected_names = selected
            .iter()
            .map(|check| check.name.as_str())
            .collect::<HashSet<_>>();
        let target_names = requested.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut reused = BTreeMap::new();
        if let Some(origin) = origin {
            for check in selected {
                let Some(previous) = origin
                    .checks
                    .iter()
                    .find(|previous| previous.name == check.name)
                else {
                    continue;
                };
                let expected = previous
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.path.as_str())
                    .collect::<Vec<_>>();
                if target_names.contains(check.name.as_str())
                    || check.completion
                        != devcoordinator2_executor_protocol::CompletionMode::Process
                    || check.command.is_none()
                    || previous.artifacts.is_empty()
                    || expected
                        != check
                            .produces
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                    || !matches!(previous.status, LeafStatus::Passed | LeafStatus::Reused)
                    || !self
                        .inner
                        .command
                        .receipts_match(
                            &self.inner.executor,
                            worktree,
                            &previous.artifacts,
                            caller.uid,
                            caller.gid,
                        )
                        .map_err(command_start_error)?
                {
                    continue;
                }
                reused.insert(check.name.clone(), previous.artifacts.clone());
            }
        }
        let checks = selected
            .iter()
            .cloned()
            .map(|mut check| -> Result<CheckPlan, ProtocolError> {
                check
                    .invalidates
                    .retain(|name| selected_names.contains(name.as_str()));
                let relative = Path::new(&check.cwd).strip_prefix(worktree).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::TestStartFailed,
                        format!(
                            "check {:?} working directory escaped the worktree",
                            check.name
                        ),
                    )
                })?;
                check.cwd = if relative.as_os_str().is_empty() {
                    ".".into()
                } else {
                    relative.to_string_lossy().into_owned()
                };
                Ok(check)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let proof = if origin_run_id.is_some() {
            ProofKind::Retry
        } else if requested.is_empty() {
            ProofKind::Complete
        } else {
            ProofKind::Selected
        };
        let plan = ExecutionPlan {
            schema: Schema2,
            run_id: run_id.into(),
            test: specification.name.clone(),
            worktree_root: worktree.to_string_lossy().into_owned(),
            current_dir: prepared.current_path.to_string_lossy().into_owned(),
            log_dir: prepared.log_path.to_string_lossy().into_owned(),
            requested_tier,
            readiness_eligible: proof == ProofKind::Complete
                && requested_tier == ValidationTier::Release,
            proof,
            selection: requested.to_vec(),
            origin_run_id: origin_run_id.map(str::to_owned),
            source_digest: source_digest.into(),
            config_digest: specification.config_digest.clone(),
            reused,
            case_selection: BTreeMap::new(),
            postgres_databases: BTreeMap::new(),
            reused_qualifications: Default::default(),
            checks,
        };
        plan.validate().map_err(|error| {
            ProtocolError::new(ErrorCode::TestStartFailed, "executor plan is invalid")
                .with_detail(error.to_string())
        })?;
        Ok(plan)
    }

    fn verify_launch(
        &self,
        handle: &RunHandle,
        process: &mut dyn UnitProcess,
        capture_failures: &[Arc<AtomicBool>],
    ) -> Result<LaunchState, ProtocolError> {
        let started = self.inner.monotonic.seconds();
        let mut mismatch = None;
        while self.inner.monotonic.seconds() - started < LAUNCH_VERIFY.as_secs_f64() {
            if capture_failures
                .iter()
                .any(|failed| failed.load(Ordering::SeqCst))
            {
                return Err(ProtocolError::new(
                    ErrorCode::TestStartFailed,
                    "executor output capture failed during launch",
                ));
            }
            let properties = self
                .inner
                .systemd
                .show_unit(&handle.unit, &["ActiveState", "MainPID", "ExecMainStatus"])
                .map_err(systemd_error)?;
            let active = property(&properties, "ActiveState");
            let pid = property(&properties, "MainPID")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            if active == Some("active") && pid > 0 {
                if let Some(uids) = self.inner.systemd.process_uids(pid)
                    && uids.iter().any(|uid| *uid != handle.caller_uid)
                {
                    mismatch = Some(uids);
                } else {
                    return Ok(LaunchState::Active);
                }
            }
            if let Some(status) = process.try_wait().map_err(|error| {
                ProtocolError::new(
                    ErrorCode::TestStartFailed,
                    "cannot observe systemd-run process",
                )
                .with_detail(error.to_string())
            })? {
                let ran = self
                    .inner
                    .systemd
                    .show_unit(&handle.unit, &["Result", "ExecMainStatus"])
                    .map_err(systemd_error)?;
                let result = property(&ran, "Result").unwrap_or("");
                let exec = property(&ran, "ExecMainStatus")
                    .and_then(|value| value.parse::<i32>().ok())
                    .unwrap_or(0);
                if status.success() || (!result.is_empty() && (1..200).contains(&exec)) {
                    return Ok(LaunchState::Exited);
                }
                return Err(ProtocolError::new(
                    ErrorCode::TestStartFailed,
                    format!("launch failed (systemd-run exit {status})"),
                ));
            }
            thread::sleep(LOOP_WAKE);
        }
        if let Some(mismatch) = mismatch {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                format!(
                    "unit process uids {mismatch:?} do not match caller {}",
                    handle.caller_uid
                ),
            ));
        }
        Err(ProtocolError::new(
            ErrorCode::TestStartFailed,
            "unit did not become active in time",
        ))
    }

    fn spawn_reaper(
        &self,
        handle: Arc<RunHandle>,
        mut process: Box<dyn UnitProcess>,
        stdout: Drain,
        stderr: Drain,
    ) {
        let lifecycle = self.clone();
        thread::spawn(move || {
            let stdout_failed = stdout.failure_flag();
            let stderr_failed = stderr.failure_flag();
            let (sender, receiver) = mpsc::sync_channel(1);
            let waiter = thread::spawn(move || {
                let _ = sender.send(process.wait());
            });
            let mut stop_requested = false;
            let exit = loop {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(exit) => break exit.ok(),
                    Err(RecvTimeoutError::Disconnected) => break None,
                    Err(RecvTimeoutError::Timeout) => {
                        if !stop_requested
                            && (stdout_failed.load(Ordering::SeqCst)
                                || stderr_failed.load(Ordering::SeqCst))
                        {
                            let _ = lifecycle.stop_unit(&handle.unit);
                            stop_requested = true;
                        }
                    }
                }
            };
            let _ = waiter.join();
            let streams_complete = stdout.finish() && stderr.finish();
            lifecycle.finalize(&handle, exit, streams_complete);
            let _ = lifecycle.inner.systemd.reset_failed(&handle.unit);
            let mut runs = lifecycle
                .inner
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if runs
                .get(&handle.worktree_id)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                runs.remove(&handle.worktree_id);
            }
        });
    }

    fn finalize(&self, handle: &RunHandle, exit: Option<ExitStatus>, streams_complete: bool) {
        let (report, report_issue) = self.report_snapshot(handle, true);
        let properties = self
            .inner
            .systemd
            .show_unit(&handle.unit, &["Result", "ExecMainStatus"])
            .unwrap_or_default();
        let requested_stop = {
            let mut state = handle
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.final_status.is_some() {
                return;
            }
            state.stop.take()
        };
        let (status, exit_code, termination) = if let Some(stop) = requested_stop {
            let termination = if stop.termination.is_some() {
                stop.termination
            } else if stop.status == TestStatus::Superseded {
                Some(devcoordinator2_api::results::RunTerminationReason::Superseded)
            } else if stop.detail.is_some() {
                Some(devcoordinator2_api::results::RunTerminationReason::OperatorCancelled)
            } else {
                None
            };
            (stop.status, None, termination)
        } else if property(&properties, "Result") == Some("timeout") {
            (
                TestStatus::TimedOut,
                None,
                Some(devcoordinator2_api::results::RunTerminationReason::TimedOut),
            )
        } else if streams_complete
            && exit.as_ref().is_some_and(ExitStatus::success)
            && report
                .as_ref()
                .is_some_and(|report| report.status == RunStatus::Passed)
        {
            (TestStatus::Passed, Some(0), None)
        } else {
            (
                TestStatus::Failed,
                exit.and_then(|status| status.code()),
                None,
            )
        };
        self.remove_containers(
            &handle
                .containers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut summary = initial_summary(
            &handle.run_id,
            &handle.test,
            &handle.started_at,
            handle.caller_uid,
            &handle.client,
            handle.proof,
            handle.selection.clone(),
            handle.origin_run_id.clone(),
            handle.requested_tier,
        );
        summary.work = handle.work.clone();
        summary.report_issue = report_issue;
        terminal_status(
            &mut summary,
            status.clone(),
            self.timestamp()
                .unwrap_or_else(|_| handle.started_at.clone()),
            (self.inner.monotonic.seconds() - handle.started_mono).max(0.0),
            exit_code,
            termination,
        );
        summary.stdout_bytes_observed = handle.stdout_bytes.load(Ordering::SeqCst);
        summary.stderr_bytes_observed = handle.stderr_bytes.load(Ordering::SeqCst);
        let mut retry_evidence_written = true;
        if let Some(report) = report
            .as_ref()
            .filter(|report| report_matches(handle, report))
        {
            let _ = apply_report(&mut summary, report);
            retry_evidence_written = self
                .inner
                .store
                .record_evidence(
                    &handle.worktree,
                    report,
                    &status,
                    summary.work.as_ref(),
                    handle.caller_uid,
                    handle.caller_gid,
                )
                .is_ok();
        }
        reconcile_terminal_checks(&mut summary);
        let summary_written = self
            .inner
            .store
            .write_summary(
                &handle.current,
                &summary,
                handle.caller_uid,
                handle.caller_gid,
            )
            .is_ok();
        let history_written = self
            .inner
            .store
            .record_history(
                &handle.worktree,
                &summary,
                handle.caller_uid,
                handle.caller_gid,
            )
            .is_ok();
        let mut state = handle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.final_status = Some(status.clone());
        state.evidence_complete = summary_written && history_written && retry_evidence_written;
        handle.finalized.notify_all();
        drop(state);
        // Publish the terminal summary before releasing admission. A
        // superseding start already holds the admission state lock; notifying
        // it first avoids a lock inversion while still keeping the prior run
        // in the activity receipt until that guard advances the successor.
        let _ = self.inner.capacity.unregister_run(&handle.run_id);
        let _ = self.inner.admission.finished(&handle.run_id);
        self.inner
            .logs
            .notify_run_finished(&handle.worktree, &handle.run_id);
        self.publish_event(TestLifecycleEvent {
            kind: "test.finished",
            run_id: handle.run_id.clone(),
            test: handle.test.clone(),
            status: Some(status),
            exit_code,
            repository_id: handle.repository_id.clone(),
            worktree_id: handle.worktree_id.clone(),
            duration_seconds: summary.duration_seconds,
            caller_uid: handle.caller_uid,
            client: handle.client.clone(),
        });
        let mut state = handle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cleanup_complete = true;
        handle.finalized.notify_all();
    }

    fn project_live(
        &self,
        summary: &mut TestSummary,
        handle: &RunHandle,
    ) -> Result<(), ProtocolError> {
        summary.stdout_bytes_observed = handle.stdout_bytes.load(Ordering::SeqCst);
        summary.stderr_bytes_observed = handle.stderr_bytes.load(Ordering::SeqCst);
        let (report, issue) = self.report_snapshot(handle, false);
        summary.report_issue = issue;
        if let Some(report) = report {
            apply_report(summary, &report).map_err(state_error)?;
        }
        if summary.report_issue.is_some() {
            summary.readiness_eligible = false;
        }
        Ok(())
    }

    fn report_snapshot(
        &self,
        handle: &RunHandle,
        terminal: bool,
    ) -> (
        Option<ExecutionReport>,
        Option<devcoordinator2_api::results::TestReportIssue>,
    ) {
        use crate::test_state::TestStateError;
        use devcoordinator2_api::results::TestReportIssue;
        match self.inner.store.read_report(&handle.current) {
            Ok(Some(report)) if !report_matches(handle, &report) => {
                (None, Some(TestReportIssue::IdentityMismatch))
            }
            Ok(Some(report)) => {
                let issue = (terminal && report.status == RunStatus::Running)
                    .then_some(TestReportIssue::Incomplete);
                (Some(report), issue)
            }
            Ok(None) => (None, terminal.then_some(TestReportIssue::Missing)),
            Err(TestStateError::Filesystem(_)) => (None, Some(TestReportIssue::Unreadable)),
            Err(TestStateError::Json(_) | TestStateError::Invalid(_)) => {
                (None, Some(TestReportIssue::Invalid))
            }
        }
    }

    fn supersede_prior(&self, worktree_id: &str) -> Result<Option<String>, ProtocolError> {
        let mut superseded_run_id = None;
        let previous = self
            .inner
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(worktree_id);
        if let Some(previous) = previous {
            {
                let mut state = previous
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.final_status.is_none() {
                    superseded_run_id = Some(previous.run_id.clone());
                    state.stop = Some(RequestedStop {
                        status: TestStatus::Superseded,
                        detail: None,
                        termination: None,
                    });
                }
            }
            self.stop_unit(&previous.unit)?;
            let state = previous
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (_state, wait) = previous
                .finalized
                .wait_timeout_while(state, STOP_WAIT, |state| state.final_status.is_none())
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if wait.timed_out() {
                return Err(ProtocolError::new(
                    ErrorCode::UnitStopFailed,
                    "superseded test did not finalize after its unit stopped",
                ));
            }
        }
        let pattern = crate::ids::unit_glob(&self.inner.config.unit_prefix, worktree_id);
        for unit in self
            .inner
            .systemd
            .list_matching_units(&pattern)
            .map_err(systemd_error)?
        {
            self.stop_unit(&unit)?;
        }
        let labels = BTreeMap::from([
            (
                "devcoordinator2.instance".into(),
                self.inner.config.unit_prefix.clone(),
            ),
            ("devcoordinator2.purpose".into(), "test".into()),
            ("devcoordinator2.worktree".into(), worktree_id.into()),
        ]);
        if let Ok(containers) = self.inner.docker.list_ids_by_labels(&labels) {
            self.remove_containers(&containers);
        }
        Ok(superseded_run_id)
    }

    fn stop_unit(&self, unit: &str) -> Result<(), ProtocolError> {
        let cgroup = self
            .inner
            .systemd
            .control_group_path(unit)
            .map_err(systemd_error)?;
        self.inner.systemd.stop_unit(unit).map_err(systemd_error)?;
        if !self
            .inner
            .systemd
            .prove_cgroup_empty(cgroup.as_deref(), Duration::from_secs(15))
        {
            return Err(ProtocolError::new(
                ErrorCode::UnitStopFailed,
                format!("cgroup of {unit} still has processes after stop"),
            ));
        }
        self.inner.systemd.reset_failed(unit).map_err(systemd_error)
    }

    fn remove_containers(&self, containers: &[ExactContainerId]) {
        for container in containers {
            let _ = self.inner.docker.remove_container(container, true);
        }
    }

    fn rollback_accepted_start(
        &self,
        worktree: &Path,
        run_id: &str,
        containers: &[ExactContainerId],
    ) {
        self.remove_containers(containers);
        let _ = self.inner.capacity.unregister_run(run_id);
        let _ = self.inner.admission.finished(run_id);
        self.cleanup_unstarted(worktree, run_id);
    }

    fn cleanup_unstarted(&self, worktree: &Path, run_id: &str) {
        let _ = self.inner.store.remove_current(worktree);
        let _ = self.inner.store.remove_log_run(worktree, run_id);
    }

    fn publish_event(&self, event: TestLifecycleEvent) {
        if let Some(sink) = self
            .inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            sink.publish(event);
        }
    }

    fn run_id(&self) -> Result<String, ProtocolError> {
        let timestamp = self
            .inner
            .clock
            .now_utc()
            .format(RUN_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format test run ID")
                    .with_detail(error.to_string())
            })?;
        let mut random = [0_u8; 3];
        self.inner.random.fill(&mut random).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot create test run ID")
                .with_detail(error.to_string())
        })?;
        Ok(format!("t{timestamp}-{}", lower_hex(&random)))
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.inner
            .clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format test timestamp")
                    .with_detail(error.to_string())
            })
    }
}

fn selected_closure(
    configured: &[CheckPlan],
    requested: &[String],
    tier: ValidationTier,
) -> Result<Vec<CheckPlan>, ProtocolError> {
    let by_name = configured
        .iter()
        .map(|check| (check.name.as_str(), check))
        .collect::<HashMap<_, _>>();
    if requested.iter().collect::<HashSet<_>>().len() != requested.len() {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "check selection contains duplicates",
        ));
    }
    let missing = requested
        .iter()
        .filter(|name| !by_name.contains_key(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("unknown checks: {}", missing.join(", ")),
        ));
    }
    let excluded = requested
        .iter()
        .filter(|name| by_name[name.as_str()].tier > tier)
        .cloned()
        .collect::<Vec<_>>();
    if !excluded.is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!(
                "checks are outside requested {} tier: {}",
                tier_name(tier),
                excluded.join(", ")
            ),
        ));
    }
    if requested.is_empty() {
        let selected = configured
            .iter()
            .filter(|check| check.tier <= tier)
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("no checks are configured for the {} tier", tier_name(tier)),
            ));
        }
        return Ok(selected);
    }
    let mut included = requested.iter().cloned().collect::<HashSet<_>>();
    let mut pending = requested.to_vec();
    while let Some(name) = pending.pop() {
        let check = by_name[&name.as_str()];
        for dependency in check.after.iter().chain(&check.requires) {
            if included.insert(dependency.clone()) {
                pending.push(dependency.clone());
            }
        }
    }
    Ok(configured
        .iter()
        .filter(|check| included.contains(&check.name) && check.tier <= tier)
        .cloned()
        .collect())
}

fn validate_retry(
    origin: &RetryEvidence,
    run_id: &str,
    check: &str,
    specification: &TestSpec,
    source_digest: &str,
) -> Result<(), ProtocolError> {
    if origin.run_id != run_id
        || origin.proof != ProofKind::Complete
        || !origin.selection.is_empty()
    {
        return Err(ProtocolError::new(
            ErrorCode::TestStartFailed,
            "a retry requires an original complete run",
        ));
    }
    if origin.test != specification.name {
        return Err(ProtocolError::new(
            ErrorCode::TestStartFailed,
            "retry evidence belongs to another test",
        ));
    }
    if origin.source_digest != source_digest || origin.config_digest != specification.config_digest
    {
        return Err(ProtocolError::new(
            ErrorCode::TestStartFailed,
            "retry evidence is stale for current source or config",
        ));
    }
    if !specification
        .checks
        .iter()
        .any(|candidate| candidate.name == check)
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("unknown check: {check}"),
        ));
    }
    if !origin.checks.iter().any(|candidate| {
        candidate.name == check
            && matches!(
                candidate.status,
                LeafStatus::Failed | LeafStatus::TimedOut | LeafStatus::Cancelled
            )
    }) {
        return Err(ProtocolError::new(
            ErrorCode::TestStartFailed,
            "only a failed, timed-out, or cancelled check from that complete run can retry",
        ));
    }
    Ok(())
}

fn bound_summary_response(summary: &mut TestSummary, budget: usize) -> Result<(), ProtocolError> {
    while serde_json::to_vec(summary)
        .map_err(|_| ProtocolError::new(ErrorCode::InternalError, "test summary encoding failed"))?
        .len()
        > budget
    {
        if let Some(checks) = &mut summary.checks
            && checks.iter().any(|check| !check.cases.is_empty())
        {
            for check in checks.iter_mut().filter(|check| !check.cases.is_empty()) {
                check.cases.truncate(check.cases.len() / 2);
                check.cases_truncated = true;
            }
        } else if let Some(failures) = &mut summary.failure_index
            && !failures.is_empty()
        {
            failures.truncate(failures.len() / 2);
            summary.failure_index_truncated = Some(true);
        } else if let Some(checks) = &mut summary.checks
            && !checks.is_empty()
        {
            checks.truncate(checks.len() / 2);
            summary.checks_truncated = Some(true);
        } else {
            return Err(ProtocolError::new(
                ErrorCode::InternalError,
                "test summary metadata exceeds response budget",
            ));
        }
    }
    Ok(())
}

fn compact_list_summary(summary: &mut TestSummary) {
    if let Some(checks) = &mut summary.checks {
        if checks.len() > 8 {
            summary.checks_truncated = Some(true);
            checks.truncate(8);
        }
        for check in checks {
            if !check.cases.is_empty() {
                check.cases_truncated = true;
                check.cases.clear();
            }
            check.streams.clear();
        }
    }
    if summary
        .failure_index
        .as_ref()
        .is_some_and(|failures| !failures.is_empty())
    {
        summary.failure_index_truncated = Some(true);
        summary.failure_index = Some(Vec::new());
    }
}

fn close_execution_observation(
    execution: Option<&mut devcoordinator2_api::results::ExecutionProgress>,
) {
    if let Some(execution) = execution {
        execution.finished += execution.waiting + execution.admitted + execution.executing;
        execution.waiting = 0;
        execution.admitted = 0;
        execution.executing = 0;
    }
}

fn reconcile_terminal_checks(summary: &mut TestSummary) {
    use devcoordinator2_api::results::LeafStatus;
    if summary.report_issue.is_some()
        || summary.termination_reason
            == Some(devcoordinator2_api::results::RunTerminationReason::MemoryPressure)
    {
        summary.readiness_eligible = false;
    }
    if summary.status == TestStatus::Running {
        return;
    }
    let interrupted = if summary.status == TestStatus::TimedOut {
        LeafStatus::TimedOut
    } else {
        LeafStatus::Cancelled
    };
    if let Some(counts) = summary.check_summary.as_mut() {
        let running = counts.insert("running".into(), 0).unwrap_or(0);
        let pending = counts.insert("pending".into(), 0).unwrap_or(0);
        *counts.entry("cancelled".into()).or_default() += pending;
        let key = if interrupted == LeafStatus::TimedOut {
            "timed_out"
        } else {
            "cancelled"
        };
        *counts.entry(key.into()).or_default() += running;
    }
    for check in summary.checks.iter_mut().flatten() {
        close_execution_observation(check.execution.as_mut());
        if matches!(check.status, LeafStatus::Pending | LeafStatus::Running) {
            check.status = if check.status == LeafStatus::Pending {
                LeafStatus::Cancelled
            } else {
                interrupted.clone()
            };
            check.finished_at.clone_from(&summary.finished_at);
        }
        for case in &mut check.cases {
            close_execution_observation(case.execution.as_mut());
            if matches!(case.status, LeafStatus::Pending | LeafStatus::Running) {
                case.status = if case.status == LeafStatus::Pending {
                    LeafStatus::Cancelled
                } else {
                    interrupted.clone()
                };
            }
        }
    }
}

fn apply_report(
    summary: &mut TestSummary,
    report: &ExecutionReport,
) -> Result<(), crate::test_state::TestStateError> {
    let all_checks = &report.checks;
    summary.requested_tier = api_tier(report.requested_tier);
    summary.readiness_eligible = report.readiness_eligible;
    summary.proof = api_proof(report.proof);
    summary.selection = report.selection.clone();
    summary.check_summary = Some(report.counts.clone());
    summary.checks = Some(
        all_checks
            .iter()
            .take(64)
            .map(api_check)
            .collect::<Result<Vec<_>, _>>()?,
    );
    summary.checks_truncated = Some(all_checks.len() > 64);
    summary.failure_index = Some(
        report
            .failure_index
            .iter()
            .take(16)
            .map(convert)
            .collect::<Result<Vec<_>, _>>()?,
    );
    summary.failure_index_truncated =
        Some(report.failure_index_truncated || report.failure_index.len() > 16);
    summary.source_changed = Some(report.source_changed);
    summary.execution_capacity = Some(ExecutionCapacity {
        learned_capacity: report.capacity.learned_capacity,
        effective_capacity: report.capacity.effective_capacity,
        capacity_wait_count: report.capacity.capacity_wait_count,
    });
    summary.capacity_wait_count = Some(report.capacity.capacity_wait_count);
    let (stdout, stderr) = aggregate_bytes(all_checks);
    summary.stdout_bytes_observed = stdout;
    summary.stderr_bytes_observed = stderr;
    Ok(())
}

fn api_check(check: &CheckReport) -> Result<CheckProjection, crate::test_state::TestStateError> {
    Ok(CheckProjection {
        execution: check.execution.as_ref().map(convert).transpose()?,
        name: check.name.clone(),
        tier: api_tier(check.tier),
        role: convert(&check.role)?,
        status: convert(&check.status)?,
        started_at: check.started_at.clone(),
        finished_at: check.finished_at.clone(),
        duration_seconds: check.duration_seconds,
        exit: ApiDiagnosticExit {
            code: check.exit.code,
            signal: check.exit.signal.map(i32::from),
        },
        streams: check
            .streams
            .iter()
            .map(convert)
            .collect::<Result<Vec<ExecutionLogStreamSummary>, _>>()?,
        case_count: check.case_count,
        artifacts: check
            .artifacts
            .iter()
            .take(8)
            .map(convert)
            .collect::<Result<Vec<ApiArtifactReceipt>, _>>()?,
        artifacts_truncated: check.artifacts.len() > 8,
        retained_artifacts: check
            .retained_artifacts
            .iter()
            .take(8)
            .map(convert)
            .collect::<Result<Vec<_>, _>>()?,
        retained_artifacts_truncated: check.retained_artifacts.len() > 8,
        cases: check
            .cases
            .iter()
            .take(32)
            .map(convert)
            .collect::<Result<Vec<CaseProjection>, _>>()?,
        cases_truncated: check.cases_truncated || check.cases.len() > 32,
    })
}

fn aggregate_bytes(checks: &[CheckReport]) -> (u64, u64) {
    let mut stdout = 0_u64;
    let mut stderr = 0_u64;
    for stream in checks.iter().flat_map(|check| check.streams.iter()).chain(
        checks
            .iter()
            .flat_map(|check| check.cases.iter())
            .flat_map(|case| case.streams.iter()),
    ) {
        match stream.log_ref.stream {
            devcoordinator2_executor_protocol::LogStream::Stdout => {
                stdout = stdout.saturating_add(stream.bytes);
            }
            devcoordinator2_executor_protocol::LogStream::Stderr => {
                stderr = stderr.saturating_add(stream.bytes);
            }
        }
    }
    (stdout, stderr)
}

fn convert<T, U>(value: &T) -> Result<U, crate::test_state::TestStateError>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|error| crate::test_state::TestStateError::Json(error.to_string()))
}

fn report_matches(handle: &RunHandle, report: &ExecutionReport) -> bool {
    report.run_id == handle.run_id
        && report.test == handle.test
        && report.proof == handle.proof
        && report.selection == handle.selection
        && report.origin_run_id == handle.origin_run_id
        && report.requested_tier == handle.requested_tier
        && report.readiness_eligible
            == (handle.proof == ProofKind::Complete
                && handle.requested_tier == ValidationTier::Release)
}

fn property<'a>(values: &'a [(String, String)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

pub(crate) fn default_executor_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/release/devcoordinator2-executor")
}

fn client_name(caller: &Caller) -> String {
    match caller.client_kind {
        devcoordinator2_api::ClientKind::Codex => "codex",
        devcoordinator2_api::ClientKind::Claude => "claude",
        devcoordinator2_api::ClientKind::Cursor => "cursor",
        devcoordinator2_api::ClientKind::Antigravity => "antigravity",
        devcoordinator2_api::ClientKind::Human => "human",
        devcoordinator2_api::ClientKind::Edge => "edge",
        devcoordinator2_api::ClientKind::Other => "other",
    }
    .into()
}

fn api_proof(proof: ProofKind) -> ApiProofKind {
    match proof {
        ProofKind::Complete => ApiProofKind::Complete,
        ProofKind::Selected => ApiProofKind::Selected,
        ProofKind::Retry => ApiProofKind::Retry,
    }
}

fn tier_name(tier: ValidationTier) -> &'static str {
    match tier {
        ValidationTier::Development => "development",
        ValidationTier::PreMerge => "pre-merge",
        ValidationTier::Release => "release",
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[usize::from(byte >> 4)] as char);
        value.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    value
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from((second & 0x0f) << 2 | third >> 6)] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0x3f)] as char);
        }
    }
    output
}

fn admission_error(error: AdmissionError) -> ProtocolError {
    let code = if matches!(error, AdmissionError::TestsDraining { .. }) {
        ErrorCode::TestsDraining
    } else {
        ErrorCode::TestStartFailed
    };
    ProtocolError::new(code, error.to_string())
}

fn config_error(error: crate::repository_config::RepositoryConfigError) -> ProtocolError {
    ProtocolError::new(ErrorCode::RepositoryConfigInvalid, error.to_string())
}

fn command_start_error(error: crate::test_command::TestCommandError) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestStartFailed, error.to_string())
}

fn postgres_error(error: crate::docker::DockerError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestStartFailed,
        format!("ephemeral PostgreSQL failed: {error}"),
    )
}

fn state_start_error(error: crate::test_state::TestStateError) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestStartFailed, error.to_string())
}

fn state_error(error: crate::test_state::TestStateError) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestLogUnavailable, error.to_string())
}

fn systemd_error(error: crate::systemd::SystemdError) -> ProtocolError {
    ProtocolError::new(ErrorCode::UnitStopFailed, error.to_string())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "test registry query failed")
            .with_detail(other.to_string()),
    }
}

fn test_not_found() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestNotFound,
        "no current test run for this worktree",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, tier: ValidationTier, dependencies: &[&str]) -> CheckPlan {
        CheckPlan {
            name: name.into(),
            tier,
            role: devcoordinator2_executor_protocol::CheckRole::Work,
            phase: devcoordinator2_executor_protocol::CheckPhase::Check,
            resources: Vec::new(),
            consumes: Vec::new(),
            cacheable: false,
            cache_inputs: Vec::new(),
            fingerprint: String::new(),
            expect_failure: false,
            qualification_of: None,
            after: dependencies.iter().map(|value| (*value).into()).collect(),
            requires: Vec::new(),
            invalidates: Vec::new(),
            cwd: ".".into(),
            env: BTreeMap::new(),
            timeout_seconds: None,
            completion: devcoordinator2_executor_protocol::CompletionMode::Process,
            on_failure: devcoordinator2_executor_protocol::FailureMode::Continue,
            produces: Vec::new(),
            retained_artifacts: Vec::new(),
            diagnostic_sources: Vec::new(),
            command: Some(vec!["true".into()]),
            discover: None,
            case_command: None,
            cases: None,
        }
    }

    #[test]
    fn selection_closes_dependencies_and_enforces_tiers_and_duplicates() {
        let checks = vec![
            check("format", ValidationTier::Development, &[]),
            check("unit", ValidationTier::PreMerge, &["format"]),
            check("release", ValidationTier::Release, &["unit"]),
        ];
        let selected =
            selected_closure(&checks, &["release".into()], ValidationTier::Release).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>(),
            ["format", "unit", "release"]
        );
        assert_eq!(
            selected_closure(&checks, &[], ValidationTier::Development)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            selected_closure(&checks, &["release".into()], ValidationTier::PreMerge)
                .unwrap_err()
                .code,
            ErrorCode::ParamsInvalid
        );
        assert!(
            selected_closure(
                &checks,
                &["unit".into(), "unit".into()],
                ValidationTier::Release
            )
            .is_err()
        );
    }
}
