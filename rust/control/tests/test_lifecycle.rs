use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::ErrorCode;
use devcoordinator2_api::params::{RetryTest, StartTest, ValidationTier as ApiValidationTier};
use devcoordinator2_api::results::{ProofKind as ApiProofKind, StopTest, TestStarted, TestStatus};
use devcoordinator2_control::access::Caller;
use devcoordinator2_control::capacity::{CapacityBroker, HostMemory};
use devcoordinator2_control::config::Config;
use devcoordinator2_control::database::Database;
use devcoordinator2_control::docker::{
    DockerControl, DockerError, DockerInvocation, DockerOutput, ExactContainerId, LogFollower,
    RunDetachedRequest,
};
use devcoordinator2_control::platform::{Clock, MonotonicClock, RandomSource};
use devcoordinator2_control::repository::Registry;
use devcoordinator2_control::systemd::{
    PersistentUnitSpec, ProcessState, SystemdControl, SystemdError, TransientUnitSpec, UnitProcess,
};
use devcoordinator2_control::test_command::{TestCommand, TestCommandError};
use devcoordinator2_control::test_lifecycle::{TestLifecycle, TestLifecycleEvent};
use devcoordinator2_control::test_logs::TestLogService;
use devcoordinator2_control::test_state::{REPORT_FILE, TestRunStore, initial_summary};
use devcoordinator2_executor_protocol::{
    ArtifactReceipt, CapacityReport, CheckReport, DiagnosticExit, ExecutionPlan, ExecutionReport,
    LeafStatus, ProofKind, RunStatus, Schema2, ValidationTier,
};
use std::os::unix::process::ExitStatusExt;
use tempfile::tempdir;
use time::{OffsetDateTime, macros::datetime};

struct FixtureClock;

impl Clock for FixtureClock {
    fn now_utc(&self) -> OffsetDateTime {
        datetime!(2026-09-04 00:00 UTC)
    }
}

struct FixtureMonotonic {
    started: Instant,
    advance: AtomicU64,
}

impl FixtureMonotonic {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            advance: AtomicU64::new(0),
        }
    }
}

impl MonotonicClock for FixtureMonotonic {
    fn seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 100.0 + self.advance.load(Ordering::SeqCst) as f64
    }
}

struct SequenceRandom(AtomicU64);

impl RandomSource for SequenceRandom {
    fn fill(&self, destination: &mut [u8]) -> Result<(), getrandom::Error> {
        let value = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        for (index, byte) in destination.iter_mut().enumerate() {
            *byte = value.wrapping_add(index as u64) as u8;
        }
        Ok(())
    }
}

struct FixtureCommand;

impl TestCommand for FixtureCommand {
    fn source_digest(
        &self,
        _executor: &Path,
        _worktree: &Path,
        _uid: u32,
        _gid: u32,
    ) -> Result<String, TestCommandError> {
        Ok("a".repeat(64))
    }

    fn receipts_match(
        &self,
        _executor: &Path,
        _worktree: &Path,
        _receipts: &[ArtifactReceipt],
        _uid: u32,
        _gid: u32,
    ) -> Result<bool, TestCommandError> {
        Ok(true)
    }

    fn executable(&self, _candidate: &Path, _uid: u32, _gid: u32) -> bool {
        true
    }

    fn executor_available(&self, _executor: &Path) -> bool {
        true
    }
}

struct FixtureDocker;

impl DockerControl for FixtureDocker {
    fn invoke(&self, _invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
        Err(DockerError::InvalidRequest("unexpected Docker call".into()))
    }

    fn spawn_follow_logs(
        &self,
        _container_id: &ExactContainerId,
    ) -> Result<LogFollower, DockerError> {
        Err(DockerError::InvalidRequest("unexpected Docker logs".into()))
    }

    fn list_ids_by_labels(
        &self,
        _labels: &BTreeMap<String, String>,
    ) -> Result<Vec<ExactContainerId>, DockerError> {
        Ok(Vec::new())
    }
}

struct PostgresDocker {
    removed: AtomicU64,
    provisioned: AtomicU64,
}

impl PostgresDocker {
    fn new() -> Self {
        Self {
            removed: AtomicU64::new(0),
            provisioned: AtomicU64::new(0),
        }
    }
}

impl DockerControl for PostgresDocker {
    fn invoke(&self, _invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
        Err(DockerError::InvalidRequest("unexpected Docker call".into()))
    }

    fn spawn_follow_logs(
        &self,
        _container_id: &ExactContainerId,
    ) -> Result<LogFollower, DockerError> {
        Err(DockerError::InvalidRequest(
            "high-level readiness fixture expected".into(),
        ))
    }

    fn available(&self) -> bool {
        true
    }

    fn ensure_digest_image(&self, _image: &str) -> Result<(), DockerError> {
        Ok(())
    }

    fn ensure_image(&self, image: &str) -> Result<(), DockerError> {
        assert_eq!(image, "postgres:18");
        Ok(())
    }

    fn run_detached(&self, request: &RunDetachedRequest) -> Result<ExactContainerId, DockerError> {
        assert_eq!(
            request.env_names,
            [
                "POSTGRES_USER",
                "POSTGRES_PASSWORD",
                "POSTGRES_DB",
                "PGDATA"
            ]
        );
        assert_eq!(request.env_values["PGDATA"], "/var/lib/postgresql/data");
        assert_eq!(request.publish, ["127.0.0.1::5432"]);
        assert_eq!(
            request.tmpfs,
            ["/var/lib/postgresql/data:rw,size=1g,mode=0700"]
        );
        assert_eq!(request.label_context.purpose, "test");
        assert_eq!(request.label_context.data_class, "disposable");
        self.provisioned.fetch_add(1, Ordering::SeqCst);
        ExactContainerId::parse("d".repeat(64))
    }

    fn published_host_port(
        &self,
        _container_id: &ExactContainerId,
        _container_port: &str,
    ) -> Result<u16, DockerError> {
        Ok(45_432)
    }

    fn wait_postgres_ready(
        &self,
        _container_id: &ExactContainerId,
        user: &str,
        database: &str,
        _timeout: Duration,
    ) -> Result<(), DockerError> {
        assert_eq!((user, database), ("app", "app_test"));
        Ok(())
    }

    fn list_ids_by_labels(
        &self,
        _labels: &BTreeMap<String, String>,
    ) -> Result<Vec<ExactContainerId>, DockerError> {
        Ok(Vec::new())
    }

    fn remove_container(
        &self,
        _container_id: &ExactContainerId,
        delete_volumes: bool,
    ) -> Result<(), DockerError> {
        assert!(delete_volumes);
        self.removed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FixtureProcessState {
    done: bool,
    exit: i32,
    uid: u32,
}

type SharedFixtureProcess = Arc<(Mutex<FixtureProcessState>, Condvar)>;

struct FixtureProcess {
    state: SharedFixtureProcess,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
}

struct ErrorReader;

impl Read for ErrorReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture capture failure"))
    }
}

impl UnitProcess for FixtureProcess {
    fn id(&self) -> u32 {
        500
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let state = self.state.0.lock().unwrap();
        Ok(state.done.then(|| ExitStatus::from_raw(state.exit << 8)))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let state = self.state.0.lock().unwrap();
        let state = self.state.1.wait_while(state, |state| !state.done).unwrap();
        Ok(ExitStatus::from_raw(state.exit << 8))
    }
}

struct FixtureSystemd {
    runs: Mutex<HashMap<String, SharedFixtureProcess>>,
    next_exit: AtomicI32,
    starts: AtomicU64,
    stops: AtomicU64,
    fail_spawn: AtomicBool,
    fail_capture: AtomicBool,
    mismatch_uid: AtomicBool,
    memory: Mutex<HashMap<String, String>>,
    fail_stop: AtomicBool,
    timed_out: AtomicBool,
    report_inactive: AtomicBool,
}

impl FixtureSystemd {
    fn new() -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
            next_exit: AtomicI32::new(0),
            starts: AtomicU64::new(0),
            stops: AtomicU64::new(0),
            fail_spawn: AtomicBool::new(false),
            fail_capture: AtomicBool::new(false),
            mismatch_uid: AtomicBool::new(false),
            memory: Mutex::new(HashMap::new()),
            fail_stop: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            report_inactive: AtomicBool::new(false),
        }
    }

    fn finish(&self, unit: &str) {
        let state = self.runs.lock().unwrap().get(unit).cloned().unwrap();
        let mut current = state.0.lock().unwrap();
        current.done = true;
        drop(current);
        state.1.notify_all();
    }

    fn seed_unit(&self, unit: &str, uid: u32) {
        self.runs.lock().unwrap().insert(
            unit.into(),
            Arc::new((
                Mutex::new(FixtureProcessState {
                    done: false,
                    exit: 1,
                    uid,
                }),
                Condvar::new(),
            )),
        );
    }

    fn write_report(specification: &TransientUnitSpec, exit: i32) {
        let plan_path = PathBuf::from(&specification.command[2]);
        let plan = ExecutionPlan::from_json(&std::fs::read(plan_path).unwrap()).unwrap();
        let passed = exit == 0;
        let leaf = if passed {
            LeafStatus::Passed
        } else {
            LeafStatus::Failed
        };
        let selected_status = if passed { "passed" } else { "failed" };
        let mut counts = BTreeMap::new();
        for name in [
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
        ] {
            counts.insert(
                name.into(),
                if name == selected_status {
                    plan.checks.len() as u32
                } else {
                    0
                },
            );
        }
        let checks = plan
            .checks
            .iter()
            .map(|check| CheckReport {
                display_name: check.display_name.clone(),
                execution: None,
                name: check.name.clone(),
                tier: check.tier,
                role: check.role,
                phase: check.phase,
                fingerprint: check.fingerprint.clone(),
                cache_inputs: Vec::new(),
                consumed_artifacts: Vec::new(),
                status: leaf,
                started_at: Some("2026-09-04T00:00:00Z".into()),
                finished_at: Some("2026-09-04T00:00:01Z".into()),
                duration_seconds: Some(1.0),
                started_epoch_ms: Some(0),
                finished_epoch_ms: Some(1000),
                exit: DiagnosticExit {
                    code: Some(exit),
                    signal: None,
                },
                artifacts: Vec::new(),
                retained_artifacts: Vec::new(),
                streams: Vec::new(),
                case_count: 0,
                cases: Vec::new(),
                cases_truncated: false,
            })
            .collect();
        let current_dir = plan.current_dir.clone();
        let report = ExecutionReport {
            schema: Schema2,
            run_id: plan.run_id,
            test: plan.test,
            requested_tier: plan.requested_tier,
            readiness_eligible: plan.readiness_eligible,
            proof: plan.proof,
            selection: plan.selection,
            origin_run_id: plan.origin_run_id,
            status: if passed {
                RunStatus::Passed
            } else {
                RunStatus::Failed
            },
            started_at: "2026-09-04T00:00:00Z".into(),
            finished_at: Some("2026-09-04T00:00:01Z".into()),
            duration_seconds: 1.0,
            source_digest: plan.source_digest,
            config_digest: plan.config_digest,
            source_changed: false,
            capacity: CapacityReport::default(),
            counts,
            checks,
            phase_durations: Vec::new(),
            failure_index: Vec::new(),
            failure_index_truncated: false,
        };
        std::fs::write(
            Path::new(&current_dir).join(REPORT_FILE),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
    }
}

impl SystemdControl for FixtureSystemd {
    fn spawn_transient(
        &self,
        specification: &TransientUnitSpec,
    ) -> Result<Box<dyn UnitProcess>, SystemdError> {
        if self.fail_spawn.load(Ordering::SeqCst) {
            return Err(SystemdError::Operation("fixture spawn failure".into()));
        }
        let exit = self.next_exit.swap(0, Ordering::SeqCst);
        Self::write_report(specification, exit);
        let state = Arc::new((
            Mutex::new(FixtureProcessState {
                done: false,
                exit,
                uid: specification.uid,
            }),
            Condvar::new(),
        ));
        self.runs
            .lock()
            .unwrap()
            .insert(specification.unit.clone(), Arc::clone(&state));
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FixtureProcess {
            state,
            stdout: Some(if self.fail_capture.load(Ordering::SeqCst) {
                Box::new(ErrorReader) as Box<dyn Read + Send>
            } else {
                Box::new(Cursor::new(b"wrapper out\n".to_vec())) as Box<dyn Read + Send>
            }),
            stderr: Some(Box::new(Cursor::new(b"wrapper err\n".to_vec()))),
        }))
    }

    fn start_persistent(&self, _specification: &PersistentUnitSpec) -> Result<(), SystemdError> {
        Err(SystemdError::Operation("unexpected persistent unit".into()))
    }

    fn process_state(&self, unit: &str) -> Result<ProcessState, SystemdError> {
        let state = self.runs.lock().unwrap().get(unit).cloned();
        let running = state
            .as_ref()
            .is_some_and(|state| !state.0.lock().unwrap().done);
        Ok(ProcessState {
            state: if running { "running" } else { "stopped" }.into(),
            active_state: if running { "active" } else { "inactive" }.into(),
            sub_state: "fixture".into(),
            result: if running { "" } else { "success" }.into(),
            main_pid: u32::from(running) * 500,
            restarts: 0,
            cgroup: "/fixture".into(),
        })
    }

    fn show_unit(
        &self,
        unit: &str,
        properties: &[&str],
    ) -> Result<Vec<(String, String)>, SystemdError> {
        let state = self.runs.lock().unwrap().get(unit).cloned();
        let (running, exit) = state.as_ref().map_or((false, 0), |state| {
            let state = state.0.lock().unwrap();
            (!state.done, state.exit)
        });
        let running = running && !self.report_inactive.load(Ordering::SeqCst);
        Ok(properties
            .iter()
            .map(|name| {
                let value = match *name {
                    "ActiveState" if running => "active".into(),
                    "ActiveState" => "inactive".into(),
                    "MainPID" if running => "500".into(),
                    "MainPID" => "0".into(),
                    "ExecMainStatus" => exit.to_string(),
                    "Result" if running => String::new(),
                    "Result" if self.timed_out.load(Ordering::SeqCst) => "timeout".into(),
                    "Result" => "success".into(),
                    "MemoryCurrent" => self
                        .memory
                        .lock()
                        .unwrap()
                        .get(unit)
                        .cloned()
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                ((*name).into(), value)
            })
            .collect())
    }

    fn list_matching_units(&self, pattern: &str) -> Result<Vec<String>, SystemdError> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .iter()
            .filter(|(unit, state)| {
                let matches = pattern
                    .split_once('*')
                    .map_or(unit.as_str() == pattern, |(prefix, suffix)| {
                        unit.starts_with(prefix) && unit.ends_with(suffix)
                    });
                matches && !state.0.lock().unwrap().done
            })
            .map(|(unit, _)| unit.clone())
            .collect())
    }

    fn stop_unit(&self, unit: &str) -> Result<(), SystemdError> {
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(SystemdError::Operation("fixture stop unavailable".into()));
        }
        self.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(state) = self.runs.lock().unwrap().get(unit).cloned() {
            let mut current = state.0.lock().unwrap();
            current.done = true;
            current.exit = 1;
            drop(current);
            state.1.notify_all();
        }
        Ok(())
    }

    fn reset_failed(&self, _unit: &str) -> Result<(), SystemdError> {
        Ok(())
    }

    fn control_group_path(&self, _unit: &str) -> Result<Option<PathBuf>, SystemdError> {
        Ok(None)
    }

    fn cgroup_memory_current(&self, cgroup: &Path) -> Option<u64> {
        self.memory
            .lock()
            .unwrap()
            .get(cgroup.file_name()?.to_str()?)?
            .parse()
            .ok()
    }

    fn prove_cgroup_empty(&self, _cgroup: Option<&Path>, _deadline: Duration) -> bool {
        true
    }

    fn process_uids(&self, pid: u32) -> Option<[u32; 4]> {
        self.runs.lock().unwrap().values().find_map(|state| {
            let state = state.0.lock().unwrap();
            (!state.done && pid == 500).then_some(
                [if self.mismatch_uid.load(Ordering::SeqCst) {
                    state.uid.saturating_add(1)
                } else {
                    state.uid
                }; 4],
            )
        })
    }
}

struct LifecycleWorld {
    _temporary: tempfile::TempDir,
    worktree: PathBuf,
    lifecycle: TestLifecycle,
    systemd: Arc<FixtureSystemd>,
    events: Arc<Mutex<Vec<TestLifecycleEvent>>>,
    events_changed: Arc<Condvar>,
    caller: Caller,
    monotonic: Arc<FixtureMonotonic>,
}

impl LifecycleWorld {
    fn new() -> Self {
        Self::with_docker(Arc::new(FixtureDocker))
    }

    fn with_docker(docker: Arc<dyn DockerControl>) -> Self {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[test.all]
[[test.all.check]]
name="unit"
tier="release"
command=["true"]
"#,
        )
        .unwrap();
        let state = temporary.path().join("state");
        let runtime = temporary.path().join("runtime");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: runtime.join("daemon.sock"),
            sandbox_bridge_dir: std::path::PathBuf::from("/tmp/devcoordinator2-bridge"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
            client_group: "clients".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: temporary.path().join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        };
        let registry = Registry::new(database.clone());
        let capacity =
            CapacityBroker::new(database.clone(), config.capacity_socket_path()).unwrap();
        let logs = TestLogService::new(database.clone(), registry.clone());
        let systemd = Arc::new(FixtureSystemd::new());
        let monotonic = Arc::new(FixtureMonotonic::new());
        let lifecycle = TestLifecycle::with_adapters(
            config,
            database,
            registry,
            capacity,
            logs,
            systemd.clone(),
            docker,
            Arc::new(FixtureCommand),
            TestRunStore,
            Arc::new(FixtureClock),
            monotonic.clone(),
            Arc::new(SequenceRandom(AtomicU64::new(0))),
            PathBuf::from("/fixture/devcoordinator2-executor"),
        )
        .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let events_changed = Arc::new(Condvar::new());
        let changed = Arc::clone(&events_changed);
        lifecycle.set_event_sink(Arc::new(move |event| {
            captured.lock().unwrap().push(event);
            changed.notify_all();
        }));
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: Some("fixture".into()),
            work: None,
            identity: None,
        };
        assert_ne!(caller.uid, 0, "lifecycle fixture requires non-root caller");
        Self {
            _temporary: temporary,
            worktree,
            lifecycle,
            systemd,
            events,
            events_changed,
            caller,
            monotonic,
        }
    }

    fn start(&self) -> TestStarted {
        self.lifecycle
            .start(
                StartTest {
                    targets: Vec::new(),
                    cases: Default::default(),
                    path: self.worktree.to_string_lossy().into_owned(),
                    test: Some("all".into()),
                    checks: Vec::new(),
                    tier: ApiValidationTier::Release,
                },
                &self.caller,
            )
            .unwrap()
    }

    fn start_other(&self) -> (PathBuf, TestStarted) {
        self.start_named("other-repository")
    }

    fn start_named(&self, name: &str) -> (PathBuf, TestStarted) {
        let path = self._temporary.path().join(name);
        std::fs::create_dir(&path).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
        std::fs::copy(
            self.worktree.join(".devcoordinator.toml"),
            path.join(".devcoordinator.toml"),
        )
        .unwrap();
        let started = self
            .lifecycle
            .start(
                StartTest {
                    targets: Vec::new(),
                    cases: Default::default(),
                    path: path.to_string_lossy().into_owned(),
                    test: Some("all".into()),
                    checks: Vec::new(),
                    tier: ApiValidationTier::Release,
                },
                &self.caller,
            )
            .unwrap();
        (path, started)
    }

    fn wait_status(&self, expected: TestStatus) -> devcoordinator2_api::results::TestSummary {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let observed = self.events.lock().unwrap().len();
            let status = self
                .lifecycle
                .status(self.worktree.to_str().unwrap(), &self.caller)
                .unwrap();
            if status.status == expected {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "status did not become {expected:?}"
            );
            let events = self.events.lock().unwrap();
            if events.len() == observed {
                let _ = self
                    .events_changed
                    .wait_timeout(events, deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
            }
        }
    }
}

#[test]
fn composed_targets_preserve_names_case_closure_and_latest_start_ownership() {
    use devcoordinator2_executor_protocol::CheckPhase;
    let world = LifecycleWorld::new();
    let mut config = String::from("schema=2\n");
    for target in ["alpha", "beta"] {
        config.push_str(&format!(
            r#"
[test.{target}]
[[test.{target}.check]]
name="setup"
tier="development"
phase="setup"
command=["true"]
[[test.{target}.check]]
name="cases"
tier="development"
phase="case"
requires=["setup"]
case_command=["true"]
cases=[{{id="first",args=[]}},{{id="second",args=[]}}]
[[test.{target}.check]]
name="cleanup"
tier="release"
phase="cleanup"
after=["cases"]
command=["true"]
"#
        ));
    }
    std::fs::write(world.worktree.join(".devcoordinator.toml"), config).unwrap();
    let request = StartTest {
        path: world.worktree.to_string_lossy().into_owned(),
        test: None,
        targets: vec!["beta".into(), "alpha".into()],
        checks: Vec::new(),
        cases: BTreeMap::new(),
        tier: ApiValidationTier::Development,
    };
    let first = world
        .lifecycle
        .start(request.clone(), &world.caller)
        .unwrap();
    assert_eq!(world.systemd.starts.load(Ordering::SeqCst), 1);
    assert_eq!(first.targets, vec!["alpha", "beta"]);
    let read_plan = || {
        ExecutionPlan::from_json(
            &std::fs::read(
                world
                    .worktree
                    .join(".devcoordinator/test/current/check-plan.json"),
            )
            .unwrap(),
        )
        .unwrap()
    };
    let composed = read_plan();
    assert_eq!(composed.checks.len(), 6);
    assert_eq!(
        composed
            .checks
            .iter()
            .map(|check| &check.name)
            .collect::<HashSet<_>>()
            .len(),
        6
    );
    assert!(
        composed
            .checks
            .iter()
            .any(|check| check.display_name.as_deref() == Some("alpha / cases"))
    );

    let mut focused = request.clone();
    focused
        .cases
        .insert("alpha/cases".into(), vec!["second".into()]);
    let second = world.lifecycle.start(focused, &world.caller).unwrap();
    assert_eq!(
        second.superseded_run_id.as_deref(),
        Some(first.run_id.as_str())
    );
    let selected = read_plan();
    assert_eq!(selected.checks.len(), 3);
    assert!(selected.checks.iter().all(|check| {
        check
            .display_name
            .as_deref()
            .unwrap()
            .starts_with("alpha / ")
    }));
    assert_eq!(
        selected
            .checks
            .iter()
            .filter(|check| check.phase == CheckPhase::Cleanup)
            .count(),
        1
    );
    assert_eq!(
        selected.case_selection.values().next().unwrap(),
        &vec!["second".to_owned()]
    );

    let mut invalid = request;
    invalid
        .cases
        .insert("alpha/cases".into(), vec!["missing".into()]);
    assert_eq!(
        world
            .lifecycle
            .start(invalid, &world.caller)
            .unwrap_err()
            .code,
        ErrorCode::ParamsInvalid
    );
    assert_eq!(
        world
            .lifecycle
            .status(world.worktree.to_str().unwrap(), &world.caller)
            .unwrap()
            .run_id,
        second.run_id
    );
    assert_eq!(world.systemd.starts.load(Ordering::SeqCst), 2);
    world.systemd.finish(&second.unit);
    let summary = world.wait_status(TestStatus::Passed);
    assert_eq!(summary.targets, vec!["alpha", "beta"]);
    assert_eq!(summary.case_selection, selected.case_selection);
}

#[test]
fn public_run_history_uses_exact_registration_without_edge_git_access() {
    let world = LifecycleWorld::new();
    let (finished, completion) = std::sync::mpsc::channel();
    world
        .lifecycle
        .set_event_sink(Arc::new(move |event: TestLifecycleEvent| {
            if event.kind == "test.finished" {
                let _ = finished.send(());
            }
        }));
    let started = world.start();
    world.systemd.finish(&started.unit);
    completion.recv_timeout(Duration::from_secs(10)).unwrap();
    let current = world.wait_status(TestStatus::Passed);
    let public = Caller {
        uid: 999,
        gid: 999,
        client_kind: devcoordinator2_api::ClientKind::Edge,
        identity: Some("administrator@example.test".into()),
        ..world.caller.clone()
    };
    let query = |path: &Path| devcoordinator2_api::params::TestHistory {
        path: path.to_string_lossy().into_owned(),
        before: None,
        limit: 20,
    };
    std::fs::rename(
        world.worktree.join(".git"),
        world._temporary.path().join("unavailable-git"),
    )
    .unwrap();
    let history = world
        .lifecycle
        .history(query(&world.worktree), &public)
        .expect("registered history must not run Git as the edge account");
    assert_eq!(history.runs.len(), 1);
    assert_eq!(history.runs[0].run_id, current.run_id);
    assert_eq!(history.runs[0].test, current.test);
    assert_eq!(history.runs[0].status, TestStatus::Passed);
    let alias = world._temporary.path().join("repository-alias");
    std::os::unix::fs::symlink(&world.worktree, &alias).unwrap();
    let unregistered = LifecycleWorld::new();
    for path in [
        world.worktree.join("nested"),
        world.worktree.join("."),
        alias,
        unregistered.worktree.clone(),
    ] {
        let error = world
            .lifecycle
            .history(query(&path), &public)
            .expect_err("public history requires an exact registered worktree");
        assert_eq!(error.code, ErrorCode::RepositoryNotFound);
        assert!(!error.message.contains(path.to_str().unwrap()));
        assert!(error.detail.is_empty());
    }
    assert_eq!(
        world
            .lifecycle
            .history(query(Path::new("relative")), &public)
            .unwrap_err()
            .code,
        ErrorCode::ParamsInvalid
    );
    assert!(
        world
            .lifecycle
            .history(query(&world.worktree), &world.caller)
            .is_err()
    );
}

#[test]
fn current_run_pages_bound_large_reports_and_keep_every_worktree_discoverable() {
    let world = LifecycleWorld::new();
    let first = world.start();
    world.systemd.finish(&first.unit);
    let mut summary = world.wait_status(TestStatus::Passed);
    let mut check = summary.checks.as_ref().unwrap()[0].clone();
    check.cases = (0..32)
        .map(|index| devcoordinator2_api::results::CaseProjection {
            execution: None,
            id: format!("case-{index}-{}", "x".repeat(96)),
            status: devcoordinator2_api::results::LeafStatus::Passed,
            exit: devcoordinator2_api::results::DiagnosticExit {
                code: Some(0),
                signal: None,
            },
            duration_ms: 1,
            streams: Vec::new(),
        })
        .collect();
    check.case_count = 32;
    summary.checks = Some(
        (0..64)
            .map(|index| {
                let mut check = check.clone();
                check.name = format!("check-{index}");
                check
            })
            .collect(),
    );
    summary
        .check_summary
        .as_mut()
        .unwrap()
        .insert("passed".into(), 64);
    let full = serde_json::to_vec(&summary).unwrap();
    assert!(full.len() > 256 * 1024 && full.len() < 2 * 1024 * 1024);
    std::fs::write(
        world
            .worktree
            .join(".devcoordinator/test/current/summary.json"),
        full,
    )
    .unwrap();
    let (other, second) = world.start_named("other-page");
    world.systemd.finish(&second.unit);
    let deadline = Instant::now() + Duration::from_secs(3);
    while world
        .lifecycle
        .status(other.to_str().unwrap(), &world.caller)
        .unwrap()
        .status
        == TestStatus::Running
    {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let mut cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = world
            .lifecycle
            .list_current_page(devcoordinator2_api::params::TestList {
                after_worktree_id: cursor,
                limit: Some(1),
            })
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < 200 * 1024);
        for row in &page.runs {
            assert!(seen.insert(row.worktree_id.clone()));
            if row.worktree_path == world.worktree.to_str().unwrap() {
                assert_eq!(row.summary.checks.as_ref().unwrap().len(), 8);
                assert_eq!(row.summary.checks_truncated, Some(true));
                assert!(row.summary.checks.as_ref().unwrap()[0].cases.is_empty());
            }
        }
        cursor = page.next_worktree_id;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 2);
    let details = world
        .lifecycle
        .status(world.worktree.to_str().unwrap(), &world.caller)
        .unwrap();
    assert_eq!(details.checks.as_ref().unwrap().len(), 64);
    assert!(serde_json::to_vec(&details).unwrap().len() < 160 * 1024);
    assert!(
        details
            .checks
            .as_ref()
            .unwrap()
            .iter()
            .all(|check| check.cases_truncated)
    );
}

#[test]
fn finished_runs_explain_missing_invalid_mismatched_and_incomplete_reports() {
    use devcoordinator2_api::results::TestReportIssue;
    for issue in [
        TestReportIssue::Missing,
        TestReportIssue::Invalid,
        TestReportIssue::IdentityMismatch,
        TestReportIssue::Incomplete,
    ] {
        let world = LifecycleWorld::new();
        let started = world.start();
        let path = world
            .worktree
            .join(".devcoordinator/test/current")
            .join(REPORT_FILE);
        match issue {
            TestReportIssue::Missing => std::fs::remove_file(&path).unwrap(),
            TestReportIssue::Invalid => std::fs::write(&path, b"{invalid private report}").unwrap(),
            _ => {
                let mut report =
                    ExecutionReport::from_json(&std::fs::read(&path).unwrap()).unwrap();
                if issue == TestReportIssue::IdentityMismatch {
                    report.run_id = "unrelated-run".into();
                } else {
                    report.status = RunStatus::Running;
                    report.finished_at = None;
                    report.checks[0].status = LeafStatus::Running;
                    report.checks[0].finished_at = None;
                    report.checks[0].duration_seconds = None;
                    report.checks[0].exit = DiagnosticExit {
                        code: None,
                        signal: None,
                    };
                    report.counts.insert("passed".into(), 0);
                    report.counts.insert("running".into(), 1);
                }
                report.validate().unwrap();
                std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
            }
        }
        world.systemd.finish(&started.unit);
        let summary = world.wait_status(TestStatus::Failed);
        assert_eq!(summary.report_issue, Some(issue));
        assert!(!summary.readiness_eligible);
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("private report")
        );
        if issue == TestReportIssue::Incomplete {
            assert_eq!(
                summary.checks.as_ref().unwrap()[0].status,
                devcoordinator2_api::results::LeafStatus::Cancelled
            );
            assert_eq!(summary.check_summary.as_ref().unwrap()["running"], 0);
        }
    }
}

#[test]
fn interrupted_executor_retains_a_retry_receipt_without_rewriting_its_report() {
    for timed_out in [true, false] {
        let world = LifecycleWorld::new();
        let started = world.start();
        let path = world
            .worktree
            .join(".devcoordinator/test/current")
            .join(REPORT_FILE);
        let mut report = ExecutionReport::from_json(&std::fs::read(&path).unwrap()).unwrap();
        report.status = RunStatus::Running;
        report.finished_at = None;
        report.checks[0].status = LeafStatus::Running;
        report.checks[0].finished_at = None;
        report.checks[0].exit = DiagnosticExit {
            code: None,
            signal: None,
        };
        report.counts.insert("passed".into(), 0);
        report.counts.insert("running".into(), 1);
        report.validate().unwrap();
        let raw = serde_json::to_vec(&report).unwrap();
        std::fs::write(&path, &raw).unwrap();
        if timed_out {
            world.systemd.timed_out.store(true, Ordering::SeqCst);
            world.systemd.finish(&started.unit);
            world.wait_status(TestStatus::TimedOut);
        } else {
            world
                .lifecycle
                .stop(
                    world.worktree.to_str().unwrap(),
                    Some("fixture cancellation".into()),
                    &world.caller,
                )
                .unwrap();
            world.wait_status(TestStatus::Cancelled);
        }
        assert_eq!(std::fs::read(&path).unwrap(), raw);
        world.systemd.timed_out.store(false, Ordering::SeqCst);
        let retry = world
            .lifecycle
            .retry(
                RetryTest {
                    path: world.worktree.display().to_string(),
                    test: Some("all".into()),
                    run_id: started.run_id,
                    check: "unit".into(),
                },
                &world.caller,
            )
            .expect("a stopped executor retains diagnostic retry evidence");
        assert_eq!(retry.proof, ApiProofKind::Retry);
        world.systemd.finish(&retry.unit);
        let completed = world.wait_status(TestStatus::Passed);
        assert_eq!(completed.proof, ApiProofKind::Retry);
        assert!(!completed.readiness_eligible);
    }
}

#[test]
fn legacy_terminal_snapshot_is_reconciled_and_can_recover_its_missing_retry_receipt() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let current = world.worktree.join(".devcoordinator/test/current");
    let mut report =
        ExecutionReport::from_json(&std::fs::read(current.join(REPORT_FILE)).unwrap()).unwrap();
    report.status = RunStatus::Running;
    report.finished_at = None;
    report.checks[0].status = LeafStatus::Running;
    report.checks[0].finished_at = None;
    report.checks[0].exit = DiagnosticExit {
        code: None,
        signal: None,
    };
    report.counts.insert("passed".into(), 0);
    report.counts.insert("running".into(), 1);
    std::fs::write(
        current.join(REPORT_FILE),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    world.systemd.timed_out.store(true, Ordering::SeqCst);
    world.systemd.finish(&started.unit);
    let mut old = world.wait_status(TestStatus::TimedOut);
    old.report_issue = None;
    old.readiness_eligible = true;
    old.checks.as_mut().unwrap()[0].status = devcoordinator2_api::results::LeafStatus::Running;
    old.check_summary
        .as_mut()
        .unwrap()
        .insert("timed_out".into(), 0);
    old.check_summary
        .as_mut()
        .unwrap()
        .insert("running".into(), 1);
    std::fs::write(
        current.join("summary.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(world.worktree.join(".devcoordinator/test/evidence.json")).unwrap();
    let status = world
        .lifecycle
        .status(world.worktree.to_str().unwrap(), &world.caller)
        .unwrap();
    assert_eq!(
        status.checks.as_ref().unwrap()[0].status,
        devcoordinator2_api::results::LeafStatus::TimedOut
    );
    assert_eq!(
        world.lifecycle.list_current().unwrap().runs[0]
            .summary
            .checks
            .as_ref()
            .unwrap()[0]
            .status,
        devcoordinator2_api::results::LeafStatus::TimedOut
    );
    world.systemd.timed_out.store(false, Ordering::SeqCst);
    let mut other = report.clone();
    other.run_id = "t20260904T000000Z-ffffff".into();
    for check in &mut other.checks {
        for stream in &mut check.streams {
            stream.log_ref.run_id.clone_from(&other.run_id);
        }
    }
    other.validate().unwrap();
    std::fs::write(
        current.join(REPORT_FILE),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    assert!(
        world
            .lifecycle
            .retry(
                RetryTest {
                    path: world.worktree.display().to_string(),
                    test: Some("all".into()),
                    run_id: started.run_id.clone(),
                    check: "unit".into()
                },
                &world.caller
            )
            .is_err()
    );
    assert!(
        !world
            .worktree
            .join(".devcoordinator/test/evidence.json")
            .exists()
    );
    std::fs::write(
        current.join(REPORT_FILE),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    let retry = world
        .lifecycle
        .retry(
            RetryTest {
                path: world.worktree.display().to_string(),
                test: Some("all".into()),
                run_id: started.run_id,
                check: "unit".into(),
            },
            &world.caller,
        )
        .expect("matching retained legacy report can restore diagnostic retry evidence");
    world.systemd.finish(&retry.unit);
    assert!(!world.wait_status(TestStatus::Passed).readiness_eligible);
}

#[test]
fn passing_checks_do_not_become_retry_targets() {
    let world = LifecycleWorld::new();
    let started = world.start();
    world.systemd.finish(&started.unit);
    world.wait_status(TestStatus::Passed);
    let result = world.lifecycle.retry(
        RetryTest {
            path: world.worktree.display().to_string(),
            test: Some("all".into()),
            run_id: started.run_id,
            check: "unit".into(),
        },
        &world.caller,
    );
    assert!(result.is_err());
}

#[test]
fn run_timeout_terminates_live_children_and_preserves_completed_checks() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let path = world
        .worktree
        .join(".devcoordinator/test/current")
        .join(REPORT_FILE);
    let mut report = ExecutionReport::from_json(&std::fs::read(&path).unwrap()).unwrap();
    let mut active = report.checks[0].clone();
    active.name = "still-running".into();
    active.status = LeafStatus::Running;
    active.finished_at = None;
    active.duration_seconds = None;
    active.exit = DiagnosticExit {
        code: None,
        signal: None,
    };
    report.checks.push(active);
    report.counts.insert("running".into(), 1);
    report.status = RunStatus::Running;
    report.finished_at = None;
    report.validate().unwrap();
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    world.systemd.timed_out.store(true, Ordering::SeqCst);
    world.systemd.finish(&started.unit);
    let summary = world.wait_status(TestStatus::TimedOut);
    assert_eq!(
        summary.checks.as_ref().unwrap()[0].status,
        devcoordinator2_api::results::LeafStatus::Passed
    );
    assert_eq!(
        summary.checks.as_ref().unwrap()[1].status,
        devcoordinator2_api::results::LeafStatus::TimedOut
    );
    assert_eq!(summary.check_summary.as_ref().unwrap()["running"], 0);
    assert_eq!(summary.check_summary.as_ref().unwrap()["timed_out"], 1);
}

#[test]
fn postgres_18_tag_uses_explicit_disposable_data_directory() {
    let docker = Arc::new(PostgresDocker::new());
    let world = LifecycleWorld::with_docker(docker.clone());
    let path = world.worktree.join(".devcoordinator.toml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(
        "\n[test.all.postgres]\nimage=\"postgres:18\"\nuser=\"app\"\ndatabase=\"app_test\"\n",
    );
    std::fs::write(path, config).unwrap();
    let started = world.start();
    assert_eq!(docker.provisioned.load(Ordering::SeqCst), 1);
    world.systemd.finish(&started.unit);
    world.wait_status(TestStatus::Passed);
    assert_eq!(docker.removed.load(Ordering::SeqCst), 1);
}

#[test]
fn memory_emergency_preserves_evidence_and_does_not_claim_operator_cancellation() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let report_path = world
        .worktree
        .join(".devcoordinator/test/current")
        .join(REPORT_FILE);
    let mut report = ExecutionReport::from_json(&std::fs::read(&report_path).unwrap()).unwrap();
    report.status = RunStatus::Running;
    report.finished_at = None;
    report.checks[0].status = LeafStatus::Running;
    report.checks[0].finished_at = None;
    report.checks[0].duration_seconds = None;
    report.checks[0].exit = DiagnosticExit {
        code: None,
        signal: None,
    };
    report.counts.insert("passed".into(), 0);
    report.counts.insert("running".into(), 1);
    report.validate().unwrap();
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    let gib = 1024_u64.pow(3);
    world
        .systemd
        .memory
        .lock()
        .unwrap()
        .insert(started.unit.clone(), (55 * gib).to_string());
    // The same large run is allowed while available memory is healthy.
    assert!(
        world
            .lifecycle
            .relieve_memory_pressure(|| Some(HostMemory {
                total: 100 * gib,
                available: 11 * gib
            }))
            .unwrap()
            .is_empty()
    );
    assert!(
        world
            .lifecycle
            .relieve_memory_pressure(|| None)
            .unwrap()
            .is_empty()
    );
    assert!(
        world
            .lifecycle
            .relieve_memory_pressure(|| Some(HostMemory {
                total: 0,
                available: 0
            }))
            .unwrap()
            .is_empty()
    );
    let stopped = world
        .lifecycle
        .relieve_memory_pressure(|| {
            Some(HostMemory {
                total: 100 * gib,
                available: 10 * gib,
            })
        })
        .unwrap();
    assert_eq!(stopped, vec![started.run_id.clone()]);
    let summary = world.wait_status(TestStatus::Failed);
    assert_eq!(
        summary.termination_reason,
        Some(devcoordinator2_api::results::RunTerminationReason::MemoryPressure)
    );
    assert!(!summary.readiness_eligible);
    assert_eq!(
        summary.checks.as_ref().unwrap()[0].status,
        devcoordinator2_api::results::LeafStatus::Cancelled
    );
    assert_eq!(summary.check_summary.as_ref().unwrap()["running"], 0);
    assert_eq!(summary.check_summary.as_ref().unwrap()["cancelled"], 1);
    let history = world
        .lifecycle
        .history(
            devcoordinator2_api::params::TestHistory {
                path: world.worktree.to_string_lossy().into_owned(),
                before: None,
                limit: 5,
            },
            &world.caller,
        )
        .unwrap();
    assert_eq!(history.runs[0].run_id, started.run_id);
    assert_eq!(history.runs[0].status, TestStatus::Failed);
    assert_eq!(
        history.runs[0].termination_reason,
        Some(devcoordinator2_api::results::RunTerminationReason::MemoryPressure)
    );
    let successor = world.start();
    world.systemd.finish(&successor.unit);
    world.wait_status(TestStatus::Passed);
    let history = world
        .lifecycle
        .history(
            devcoordinator2_api::params::TestHistory {
                path: world.worktree.to_string_lossy().into_owned(),
                before: None,
                limit: 5,
            },
            &world.caller,
        )
        .unwrap();
    assert_eq!(
        history
            .runs
            .iter()
            .find(|run| run.run_id == started.run_id)
            .unwrap()
            .termination_reason,
        Some(devcoordinator2_api::results::RunTerminationReason::MemoryPressure)
    );
    assert!(
        world
            .lifecycle
            .relieve_memory_pressure(|| Some(HostMemory {
                total: 100 * gib,
                available: 0
            }))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn memory_emergency_does_not_blame_missing_small_or_foreign_measurements() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let gib = 1024_u64.pow(3);
    let pressure = || {
        Some(HostMemory {
            total: 100 * gib,
            available: gib,
        })
    };
    world.systemd.seed_unit("foreign.service", world.caller.uid);
    world
        .systemd
        .memory
        .lock()
        .unwrap()
        .insert("foreign.service".into(), (90 * gib).to_string());
    for value in ["", "[not set]", "18446744073709551616", "1024"] {
        world
            .systemd
            .memory
            .lock()
            .unwrap()
            .insert(started.unit.clone(), value.into());
        assert!(
            world
                .lifecycle
                .relieve_memory_pressure(pressure)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            world
                .systemd
                .process_state(&started.unit)
                .unwrap()
                .active_state,
            "active"
        );
        assert_eq!(
            world
                .systemd
                .process_state("foreign.service")
                .unwrap()
                .active_state,
            "active"
        );
    }
    world.systemd.finish(&started.unit);
    world.wait_status(TestStatus::Passed);
    world.systemd.finish("foreign.service");
}

#[test]
fn memory_emergency_retries_the_exact_run_after_a_failed_stop() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let gib = 1024_u64.pow(3);
    let pressure = || {
        Some(HostMemory {
            total: 100 * gib,
            available: 5 * gib,
        })
    };
    world
        .systemd
        .memory
        .lock()
        .unwrap()
        .insert(started.unit.clone(), (50 * gib).to_string());
    world.systemd.fail_stop.store(true, Ordering::SeqCst);
    assert!(world.lifecycle.relieve_memory_pressure(pressure).is_err());
    assert_eq!(
        world
            .systemd
            .process_state(&started.unit)
            .unwrap()
            .active_state,
        "active"
    );
    world.systemd.fail_stop.store(false, Ordering::SeqCst);
    assert_eq!(
        world.lifecycle.relieve_memory_pressure(pressure).unwrap(),
        vec![started.run_id]
    );
    world.wait_status(TestStatus::Failed);
}

#[test]
fn memory_emergency_selects_largest_then_stops_when_host_recovers() {
    let world = LifecycleWorld::new();
    let smaller = world.start();
    let (other_path, larger) = world.start_other();
    let gib = 1024_u64.pow(3);
    world.systemd.memory.lock().unwrap().extend([
        (smaller.unit.clone(), (10 * gib).to_string()),
        (larger.unit.clone(), (60 * gib).to_string()),
    ]);
    let stopped = world
        .lifecycle
        .relieve_memory_pressure(|| {
            let large_active = world
                .systemd
                .process_state(&larger.unit)
                .unwrap()
                .active_state
                == "active";
            Some(HostMemory {
                total: 100 * gib,
                available: if large_active { 5 * gib } else { 65 * gib },
            })
        })
        .unwrap();
    assert_eq!(stopped, vec![larger.run_id]);
    assert_eq!(
        world
            .lifecycle
            .status(other_path.to_str().unwrap(), &world.caller)
            .unwrap()
            .status,
        TestStatus::Failed
    );
    assert_eq!(
        world
            .systemd
            .process_state(&smaller.unit)
            .unwrap()
            .active_state,
        "active"
    );
    world.systemd.finish(&smaller.unit);
    world.wait_status(TestStatus::Passed);
}

#[test]
fn memory_emergency_does_not_stop_a_replacement_run_from_an_old_snapshot() {
    let world = LifecycleWorld::new();
    let original = world.start();
    let gib = 1024_u64.pow(3);
    world
        .systemd
        .memory
        .lock()
        .unwrap()
        .insert(original.unit.clone(), (60 * gib).to_string());
    let mut samples = 0;
    let mut replacement = None;
    let stopped = world
        .lifecycle
        .relieve_memory_pressure(|| {
            samples += 1;
            if samples == 2 {
                replacement = Some(world.start());
            }
            Some(HostMemory {
                total: 100 * gib,
                available: 5 * gib,
            })
        })
        .unwrap();
    assert!(stopped.is_empty());
    let replacement = replacement.unwrap();
    assert_eq!(
        world
            .systemd
            .process_state(&replacement.unit)
            .unwrap()
            .active_state,
        "active"
    );
    world.systemd.finish(&replacement.unit);
    world.wait_status(TestStatus::Passed);
}

#[test]
fn memory_emergency_counts_and_cleans_up_owned_database_containers() {
    let docker = Arc::new(PostgresDocker::new());
    let world = LifecycleWorld::with_docker(docker.clone());
    let configuration = world.worktree.join(".devcoordinator.toml");
    let mut text = std::fs::read_to_string(&configuration).unwrap();
    text.push_str(&format!("\n[test.all.postgres]\nimage=\"postgres@sha256:{}\"\nuser=\"app\"\ndatabase=\"app_test\"\n", "a".repeat(64)));
    std::fs::write(configuration, text).unwrap();
    let started = world.start();
    let gib = 1024_u64.pow(3);
    world.systemd.memory.lock().unwrap().extend([
        (started.unit.clone(), "1024".into()),
        (
            format!("docker-{}.scope", "d".repeat(64)),
            (60 * gib).to_string(),
        ),
        (
            format!("docker-{}.scope", "f".repeat(64)),
            (90 * gib).to_string(),
        ),
    ]);
    assert_eq!(
        world
            .lifecycle
            .relieve_memory_pressure(|| Some(HostMemory {
                total: 100 * gib,
                available: 5 * gib
            }))
            .unwrap(),
        vec![started.run_id]
    );
    assert_eq!(docker.provisioned.load(Ordering::SeqCst), 1);
    assert_eq!(docker.removed.load(Ordering::SeqCst), 1);
    world.wait_status(TestStatus::Failed);
}

#[test]
fn memory_emergency_reports_unretained_evidence_without_leaving_the_process_running() {
    let world = LifecycleWorld::new();
    let started = world.start();
    let gib = 1024_u64.pow(3);
    world
        .systemd
        .memory
        .lock()
        .unwrap()
        .insert(started.unit.clone(), (60 * gib).to_string());
    let outside = world._temporary.path().join("preserved-history.json");
    std::fs::write(&outside, "preserve").unwrap();
    std::os::unix::fs::symlink(
        &outside,
        world.worktree.join(".devcoordinator/test/history.json"),
    )
    .unwrap();
    let error = world
        .lifecycle
        .relieve_memory_pressure(|| {
            Some(HostMemory {
                total: 100 * gib,
                available: 5 * gib,
            })
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::UnitStopFailed);
    assert!(error.message.contains("evidence could not be retained"));
    assert_eq!(
        world
            .systemd
            .process_state(&started.unit)
            .unwrap()
            .active_state,
        "inactive"
    );
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "preserve");
    world.wait_status(TestStatus::Failed);
}

#[test]
fn work_context_survives_governed_run_completion_and_retained_evidence() {
    use devcoordinator2_api::work_context::{WorkContext, WorkSource};
    let mut world = LifecycleWorld::new();
    let context = devcoordinator2_api::ClientContext {
        session: Some("fixture".into()),
        work: Some(WorkContext::parse(r#"{"version":1,"native_project_id":"native-clock-not-coordinator-id","thread_id":"fixture","turn_id":"turn-1","operation_id":"operation-1","workstream_id":"implementation","outcome_id":"outcome-1","experiment_ref":"review-1@2"}"#).unwrap()),
        work_source: Some(WorkSource::Environment),
        ..Default::default()
    };
    world.caller.work = context.work_attribution();
    let expected = world.caller.work.clone();
    let (finished, completion) = std::sync::mpsc::channel();
    world
        .lifecycle
        .set_event_sink(Arc::new(move |event: TestLifecycleEvent| {
            if event.kind == "test.finished" {
                let _ = finished.send(());
            }
        }));
    let started = world.start();
    assert_ne!(started.repository_id, "native-clock-not-coordinator-id");
    assert_eq!(
        world
            .lifecycle
            .status(world.worktree.to_str().unwrap(), &world.caller)
            .unwrap()
            .work,
        expected
    );
    world.systemd.finish(&started.unit);
    completion.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(world.wait_status(TestStatus::Passed).work, expected);
    let history = world
        .lifecycle
        .history(
            devcoordinator2_api::params::TestHistory {
                path: world.worktree.to_string_lossy().into_owned(),
                before: None,
                limit: 1,
            },
            &world.caller,
        )
        .unwrap();
    assert_eq!(history.runs[0].work, expected);
    assert_eq!(
        TestRunStore.read_history(&world.worktree).unwrap()[0].work,
        expected
    );
    assert_eq!(
        TestRunStore.read_evidence(&world.worktree).unwrap()[0].work,
        expected
    );
}

#[test]
fn work_context_history_pages_preserve_whole_contexts_and_advance() {
    use devcoordinator2_api::work_context::{WorkAttribution, WorkContext, WorkSource};
    let world = LifecycleWorld::new();
    let context = WorkContext {
        version: 1,
        native_project_id: "n".repeat(256),
        thread_id: "t".repeat(256),
        turn_id: Some("u".repeat(256)),
        operation_id: Some("o".repeat(256)),
        workstream_id: Some("w".repeat(256)),
        outcome_id: Some("d".repeat(256)),
        experiment_ref: Some("e".repeat(256)),
    };
    context.validate().unwrap();
    let work = WorkAttribution {
        context: Some(context),
        source: WorkSource::Environment,
        diagnostic: None,
    };
    let directory = world.worktree.join(".devcoordinator/test");
    std::fs::create_dir_all(&directory).unwrap();
    let rows = (0..60).map(|index| serde_json::json!({"work":work,"run_id":format!("t20260904T000000Z-{index:06x}"),"test":"all","status":"passed","started_at":"2026-09-04T00:00:00Z","finished_at":"2026-09-04T00:00:01Z","duration_seconds":1.0,"exit_code":0})).collect::<Vec<_>>();
    std::fs::write(
        directory.join("history.json"),
        serde_json::to_vec(&serde_json::json!({"schema":2,"runs":rows})).unwrap(),
    )
    .unwrap();
    let query = |before| devcoordinator2_api::params::TestHistory {
        path: world.worktree.to_string_lossy().into_owned(),
        before,
        limit: 50,
    };
    let first = world.lifecycle.history(query(None), &world.caller).unwrap();
    assert!(first.runs.len() < 50);
    assert!(first.next_before.is_some());
    assert!(serde_json::to_vec(&first.runs).unwrap().len() < 12_288);
    assert_eq!(first.runs[0].work.as_ref(), Some(&work));
    let next = world
        .lifecycle
        .history(query(first.next_before), &world.caller)
        .unwrap();
    assert_ne!(first.runs[0].run_id, next.runs[0].run_id);
    assert_eq!(next.runs[0].work.as_ref(), Some(&work));
}

#[test]
fn run_history_is_bounded_scoped_and_keeps_earlier_identity() {
    let world = LifecycleWorld::new();
    let query = |before: Option<String>, limit| devcoordinator2_api::params::TestHistory {
        path: world.worktree.to_string_lossy().into_owned(),
        before,
        limit,
    };
    assert!(
        world
            .lifecycle
            .history(query(None, 20), &world.caller)
            .unwrap()
            .runs
            .is_empty()
    );
    assert_eq!(
        world
            .lifecycle
            .history(query(None, 0), &world.caller)
            .unwrap_err()
            .code,
        ErrorCode::ParamsInvalid
    );
    assert_eq!(
        world
            .lifecycle
            .history(query(None, 51), &world.caller)
            .unwrap_err()
            .code,
        ErrorCode::ParamsInvalid
    );
    let (finished, completion) = std::sync::mpsc::channel();
    world
        .lifecycle
        .set_event_sink(Arc::new(move |event: TestLifecycleEvent| {
            if event.kind == "test.finished" {
                let _ = finished.send(());
            }
        }));
    let started = world.start();
    let running = world
        .lifecycle
        .history(query(None, 1), &world.caller)
        .unwrap();
    assert_eq!(running.runs[0].run_id, started.run_id);
    assert_eq!(running.runs[0].status, TestStatus::Running);
    world.systemd.finish(&started.unit);
    completion.recv_timeout(Duration::from_secs(10)).unwrap();
    let current = world.wait_status(TestStatus::Passed);
    for (run_id, timestamp, name) in [
        (
            "t20260101T000000Z-abc123",
            "2026-01-01T00:00:00Z",
            "native-editor",
        ),
        (
            "t20251231T000000Z-abc122",
            "2025-12-31T00:00:00Z",
            "managed-planner",
        ),
    ] {
        let mut earlier = current.clone();
        earlier.run_id = run_id.to_owned();
        earlier.started_at = timestamp.to_owned();
        earlier.test = name.to_owned();
        devcoordinator2_control::test_state::TestRunStore
            .record_history(
                &world.worktree,
                &earlier,
                world.caller.uid,
                world.caller.gid,
            )
            .unwrap();
    }
    let first = world
        .lifecycle
        .history(query(None, 1), &world.caller)
        .unwrap();
    assert_eq!(first.runs.len(), 1);
    assert_eq!(first.runs[0].run_id, current.run_id);
    let second = world
        .lifecycle
        .history(query(first.next_before, 1), &world.caller)
        .unwrap();
    assert_eq!(second.runs[0].test, "native-editor");
    assert_eq!(second.runs[0].started_at, "2026-01-01T00:00:00Z");
    let third = world
        .lifecycle
        .history(query(second.next_before, 1), &world.caller)
        .unwrap();
    assert_eq!(third.runs[0].test, "managed-planner");
    assert!(third.next_before.is_none());
    let all = world
        .lifecycle
        .history(query(None, 50), &world.caller)
        .unwrap();
    assert_eq!(all.runs.len(), 3);
    let serialized = serde_json::to_string(&all).unwrap();
    assert!(!serialized.contains(world.worktree.to_str().unwrap()));
    assert!(!serialized.contains("stdout"));
    assert!(!serialized.contains("checks"));
    assert_eq!(
        world
            .lifecycle
            .history(
                query(Some("t20250101T000000Z-ffff00".into()), 1),
                &world.caller
            )
            .unwrap_err()
            .code,
        ErrorCode::ParamsInvalid
    );
    let other = LifecycleWorld::new();
    assert!(
        other
            .lifecycle
            .history(
                devcoordinator2_api::params::TestHistory {
                    path: other.worktree.to_string_lossy().into_owned(),
                    before: None,
                    limit: 20,
                },
                &other.caller
            )
            .unwrap()
            .runs
            .is_empty()
    );
}

#[test]
fn lifecycle_completes_cancels_supersedes_lists_and_retries() {
    let world = LifecycleWorld::new();
    let first = world.start();
    let running = world
        .lifecycle
        .status(world.worktree.to_str().unwrap(), &world.caller)
        .unwrap();
    assert_eq!(running.status, TestStatus::Running);
    assert_eq!(running.check_summary.as_ref().unwrap()["passed"], 1);
    assert!(running.capacity.is_some());
    world.systemd.finish(&first.unit);
    let passed = world.wait_status(TestStatus::Passed);
    assert_eq!(passed.stdout_bytes_observed, 0);
    assert_eq!(
        passed.checks.as_ref().unwrap()[0].status,
        devcoordinator2_api::results::LeafStatus::Passed
    );
    assert_eq!(world.lifecycle.list_current().unwrap().runs.len(), 1);

    let cancelled = world.start();
    let stopped = world
        .lifecycle
        .stop(
            world.worktree.to_str().unwrap(),
            Some("owner requested stop".into()),
            &world.caller,
        )
        .unwrap();
    assert!(matches!(
        stopped,
        StopTest::Cancelled {
            status: TestStatus::Cancelled,
            ..
        }
    ));
    assert_eq!(
        world.wait_status(TestStatus::Cancelled).run_id,
        cancelled.run_id
    );

    let superseded = world.start();
    let successor = world.start();
    assert_eq!(superseded.superseded_run_id, None);
    assert_ne!(superseded.run_id, successor.run_id);
    assert_eq!(
        successor.superseded_run_id.as_deref(),
        Some(superseded.run_id.as_str())
    );
    assert!(world.systemd.stops.load(Ordering::SeqCst) >= 2);
    world.systemd.finish(&successor.unit);
    world.wait_status(TestStatus::Passed);

    world.systemd.next_exit.store(1, Ordering::SeqCst);
    let failed = world.start();
    world.systemd.finish(&failed.unit);
    world.wait_status(TestStatus::Failed);
    let retry = world
        .lifecycle
        .retry(
            RetryTest {
                path: world.worktree.to_string_lossy().into_owned(),
                test: Some("all".into()),
                run_id: failed.run_id.clone(),
                check: "unit".into(),
            },
            &world.caller,
        )
        .unwrap();
    assert_eq!(retry.proof, ApiProofKind::Retry);
    assert_eq!(retry.origin_run_id.as_deref(), Some(failed.run_id.as_str()));
    world.systemd.finish(&retry.unit);
    world.wait_status(TestStatus::Passed);
    assert!(TestRunStore.read_history(&world.worktree).unwrap().len() >= 5);
    let events = world.events.lock().unwrap();
    assert!(events.iter().any(|event| event.kind == "test.started"));
    assert!(events.iter().any(|event| {
        event.kind == "test.finished" && event.status == Some(TestStatus::Cancelled)
    }));
    drop(events);

    world.systemd.fail_spawn.store(true, Ordering::SeqCst);
    let failed_launch = world.lifecycle.start(
        StartTest {
            targets: Vec::new(),
            cases: Default::default(),
            path: world.worktree.to_string_lossy().into_owned(),
            test: Some("all".into()),
            checks: Vec::new(),
            tier: ApiValidationTier::Release,
        },
        &world.caller,
    );
    assert_eq!(
        failed_launch.unwrap_err().code,
        devcoordinator2_api::ErrorCode::TestStartFailed
    );
    assert_eq!(
        world
            .lifecycle
            .status(world.worktree.to_str().unwrap(), &world.caller)
            .unwrap_err()
            .code,
        devcoordinator2_api::ErrorCode::TestNotFound
    );

    world.systemd.fail_spawn.store(false, Ordering::SeqCst);
    world.systemd.fail_capture.store(true, Ordering::SeqCst);
    let stops_before = world.systemd.stops.load(Ordering::SeqCst);
    let capture_start = world.lifecycle.start(
        StartTest {
            targets: Vec::new(),
            cases: Default::default(),
            path: world.worktree.to_string_lossy().into_owned(),
            test: Some("all".into()),
            checks: Vec::new(),
            tier: ApiValidationTier::Release,
        },
        &world.caller,
    );
    match capture_start {
        Ok(started) => {
            let failed = world.wait_status(TestStatus::Failed);
            assert_eq!(failed.run_id, started.run_id);
        }
        Err(error) => {
            assert_eq!(error.code, devcoordinator2_api::ErrorCode::TestStartFailed);
            world.wait_status(TestStatus::Failed);
        }
    }
    assert!(world.systemd.stops.load(Ordering::SeqCst) > stops_before);

    world.systemd.fail_capture.store(false, Ordering::SeqCst);
    world.systemd.mismatch_uid.store(true, Ordering::SeqCst);
    let mismatch = world.lifecycle.start(
        StartTest {
            targets: Vec::new(),
            cases: Default::default(),
            path: world.worktree.to_string_lossy().into_owned(),
            test: Some("all".into()),
            checks: Vec::new(),
            tier: ApiValidationTier::Release,
        },
        &world.caller,
    );
    assert_eq!(
        mismatch.unwrap_err().code,
        devcoordinator2_api::ErrorCode::TestStartFailed
    );
    world.wait_status(TestStatus::Failed);
}

#[test]
fn lifecycle_provisions_private_ephemeral_postgres_and_removes_it_on_finish() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempdir().unwrap();
    let worktree = temporary.path().join("repository");
    std::fs::create_dir(&worktree).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&worktree)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        worktree.join(".devcoordinator.toml"),
        format!(
            r#"
schema=2
[test.database]
[[test.database.check]]
name="unit"
tier="release"
command=["true"]
[test.database.postgres]
image="postgres@sha256:{}"
user="app"
database="app_test"
"#,
            "a".repeat(64)
        ),
    )
    .unwrap();
    let state = temporary.path().join("state");
    let runtime = temporary.path().join("runtime");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    let database = Database::open(state.join("authority.sqlite3")).unwrap();
    let config = Config {
        socket_path: runtime.join("daemon.sock"),
        sandbox_bridge_dir: std::path::PathBuf::from("/tmp/devcoordinator2-bridge"),
        state_dir: state,
        unit_prefix: "devcoordinator2-test".into(),
        slice_name: "devcoordinator2-tests.slice".into(),
        client_group: "clients".into(),
        port_range: (40000, 40100),
        base_domain: "example.test".into(),
        edge_uid: None,
        admin_emails: Vec::new(),
        telegram_token_file: None,
        telegram_api: "https://api.telegram.org".into(),
        bugs_dir: temporary.path().join("bugs"),
        compose_env_allowlist_file: None,
        compose_env_authorizations: HashSet::new(),
        codex_usage_sources_file: None,
        codex_usage_sources: Vec::new(),
    };
    let registry = Registry::new(database.clone());
    let capacity = CapacityBroker::new(database.clone(), config.capacity_socket_path()).unwrap();
    let logs = TestLogService::new(database.clone(), registry.clone());
    let systemd = Arc::new(FixtureSystemd::new());
    let docker = Arc::new(PostgresDocker::new());
    let lifecycle = TestLifecycle::with_adapters(
        config,
        database,
        registry,
        capacity,
        logs,
        systemd.clone(),
        docker.clone(),
        Arc::new(FixtureCommand),
        TestRunStore,
        Arc::new(FixtureClock),
        Arc::new(FixtureMonotonic::new()),
        Arc::new(SequenceRandom(AtomicU64::new(0))),
        PathBuf::from("/fixture/devcoordinator2-executor"),
    )
    .unwrap();
    let caller = Caller {
        pid: 1,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        client_kind: devcoordinator2_api::ClientKind::Codex,
        client_session: None,
        work: None,
        identity: None,
    };
    assert_ne!(caller.uid, 0, "PostgreSQL lifecycle fixture needs non-root");
    let started = lifecycle
        .start(
            StartTest {
                targets: Vec::new(),
                cases: Default::default(),
                path: worktree.to_string_lossy().into_owned(),
                test: Some("database".into()),
                checks: Vec::new(),
                tier: ApiValidationTier::Release,
            },
            &caller,
        )
        .unwrap();
    assert_eq!(docker.provisioned.load(Ordering::SeqCst), 1);
    let environment = worktree.join(".devcoordinator/test/current/env");
    assert_eq!(
        std::fs::metadata(&environment)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let private_environment = std::fs::read_to_string(&environment).unwrap();
    assert!(private_environment.contains("PGPASSWORD="));
    let summary = lifecycle
        .status(worktree.to_str().unwrap(), &caller)
        .unwrap();
    let public = serde_json::to_string(&summary).unwrap();
    assert!(!public.contains("PGPASSWORD"));
    assert!(!public.contains("postgresql://"));

    systemd.finish(&started.unit);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = lifecycle
            .status(worktree.to_str().unwrap(), &caller)
            .unwrap();
        if status.status == TestStatus::Passed {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(docker.removed.load(Ordering::SeqCst), 1);
}

#[test]
fn work_context_recovery_skips_unavailable_worktree_and_records_interrupted_summary() {
    let temporary = tempdir().unwrap();
    let worktree = temporary.path().join("repository");
    std::fs::create_dir(&worktree).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&worktree)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .status()
            .unwrap()
            .success()
    );
    let state = temporary.path().join("state");
    let runtime = temporary.path().join("runtime");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    let database = Database::open(state.join("authority.sqlite3")).unwrap();
    let config = Config {
        socket_path: runtime.join("daemon.sock"),
        sandbox_bridge_dir: std::path::PathBuf::from("/tmp/devcoordinator2-bridge"),
        state_dir: state,
        unit_prefix: "devcoordinator2-test".into(),
        slice_name: "devcoordinator2-tests.slice".into(),
        client_group: "clients".into(),
        port_range: (40000, 40100),
        base_domain: "example.test".into(),
        edge_uid: None,
        admin_emails: Vec::new(),
        telegram_token_file: None,
        telegram_api: "https://api.telegram.org".into(),
        bugs_dir: temporary.path().join("bugs"),
        compose_env_allowlist_file: None,
        compose_env_authorizations: HashSet::new(),
        codex_usage_sources_file: None,
        codex_usage_sources: Vec::new(),
    };
    let caller = Caller {
        pid: 1,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        client_kind: devcoordinator2_api::ClientKind::Other,
        client_session: None,
        work: None,
        identity: None,
    };
    let registry = Registry::new(database.clone());
    let registered = registry
        .register(&worktree, caller.uid, caller.gid)
        .unwrap();
    let unavailable = temporary.path().join("unavailable-repository");
    std::fs::create_dir(&unavailable).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&unavailable)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .status()
            .unwrap()
            .success()
    );
    registry
        .register(&unavailable, caller.uid, caller.gid)
        .unwrap();
    std::fs::remove_dir_all(&unavailable).unwrap();
    let run_id = "t20260904T000000Z-aabbcc";
    let store = TestRunStore;
    let prepared = store
        .prepare(
            &worktree.canonicalize().unwrap(),
            run_id,
            caller.uid,
            caller.gid,
        )
        .unwrap();
    let mut summary = initial_summary(
        run_id,
        "all",
        "2026-09-04T00:00:00Z",
        caller.uid,
        "other",
        ProofKind::Complete,
        Vec::new(),
        None,
        ValidationTier::Release,
    );
    summary.work = devcoordinator2_api::ClientContext {
        work: Some(devcoordinator2_api::work_context::WorkContext::parse(r#"{"version":1,"native_project_id":"native-clock","thread_id":"thread-restore","outcome_id":"outcome-restore"}"#).unwrap()),
        ..Default::default()
    }.work_attribution();
    let expected_work = summary.work.clone();
    store
        .write_summary(&prepared.current, &summary, caller.uid, caller.gid)
        .unwrap();
    let systemd = Arc::new(FixtureSystemd::new());
    let unit = devcoordinator2_control::ids::unit_name(
        &config.unit_prefix,
        &registered.worktree_id,
        "stale",
    );
    systemd.seed_unit(&unit, caller.uid);
    let capacity = CapacityBroker::new(database.clone(), config.capacity_socket_path()).unwrap();
    let logs = TestLogService::new(database.clone(), registry.clone());
    let lifecycle = TestLifecycle::with_adapters(
        config,
        database,
        registry,
        capacity,
        logs,
        systemd.clone(),
        Arc::new(FixtureDocker),
        Arc::new(FixtureCommand),
        store,
        Arc::new(FixtureClock),
        Arc::new(FixtureMonotonic::new()),
        Arc::new(SequenceRandom(AtomicU64::new(0))),
        PathBuf::from("/fixture/devcoordinator2-executor"),
    )
    .unwrap();
    lifecycle.recover().unwrap();
    let interrupted = lifecycle
        .status(worktree.to_str().unwrap(), &caller)
        .unwrap();
    assert_eq!(interrupted.status, TestStatus::Interrupted);
    assert_eq!(interrupted.work, expected_work);
    assert_eq!(
        interrupted.termination_reason,
        Some(devcoordinator2_api::results::RunTerminationReason::Interrupted)
    );
    assert_eq!(store.read_history(&worktree).unwrap().len(), 1);
    assert_eq!(
        store.read_history(&worktree).unwrap()[0].work,
        expected_work
    );
    assert!(systemd.stops.load(Ordering::SeqCst) >= 1);
}

#[test]
fn collected_unit_allows_its_live_reaper_to_finish_successfully() {
    let world = LifecycleWorld::new();
    let run = world.start();
    // Model systemd collecting the unit before systemd-run's wait completes.
    world.systemd.report_inactive.store(true, Ordering::SeqCst);
    world.lifecycle.reconcile_orphans().unwrap();
    world.lifecycle.reconcile_orphans().unwrap();
    assert_eq!(world.systemd.stops.load(Ordering::SeqCst), 0);
    world.systemd.finish(&run.unit);
    let summary = world.wait_status(TestStatus::Passed);
    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.termination_reason, None);
}

#[test]
fn stalled_reaper_is_interrupted_after_the_completion_window() {
    let world = LifecycleWorld::new();
    let run = world.start();
    world.systemd.report_inactive.store(true, Ordering::SeqCst);
    world.lifecycle.reconcile_orphans().unwrap();
    assert_eq!(world.systemd.stops.load(Ordering::SeqCst), 0);
    world.monotonic.advance.fetch_add(11, Ordering::SeqCst);
    world.lifecycle.reconcile_orphans().unwrap();
    world.wait_status(TestStatus::Interrupted);
    world.lifecycle.reconcile_orphans().unwrap();
    assert_eq!(world.systemd.stops.load(Ordering::SeqCst), 1);
    assert_eq!(TestRunStore.read_history(&world.worktree).unwrap().len(), 1);
    world.systemd.report_inactive.store(false, Ordering::SeqCst);
    let next = world.start();
    assert!(next.superseded_run_id.is_none());
    assert_ne!(next.run_id, run.run_id);
    world.systemd.finish(&next.unit);
    world.wait_status(TestStatus::Passed);
}

#[test]
fn reconciler_preserves_a_live_supervised_test() {
    let world = LifecycleWorld::new();
    let run = world.start();
    world.lifecycle.reconcile_orphans().unwrap();
    assert_eq!(world.systemd.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        world
            .lifecycle
            .status(world.worktree.to_str().unwrap(), &world.caller)
            .unwrap()
            .status,
        TestStatus::Running
    );
    world.systemd.finish(&run.unit);
}

#[test]
fn reconciler_interrupts_unsupervised_state_once_and_preserves_history() {
    let world = LifecycleWorld::new();
    let run = world.start();
    world.systemd.finish(&run.unit);
    // Wait for the actual fixture completion, then simulate a lost durable
    // terminal write. Reconciliation must not resurrect or repeat the run.
    let deadline = Instant::now() + Duration::from_secs(3);
    while world
        .lifecycle
        .status(world.worktree.to_str().unwrap(), &world.caller)
        .unwrap()
        .status
        == TestStatus::Running
    {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let store = TestRunStore;
    let current = store.open_current(&world.worktree).unwrap().unwrap();
    let mut summary = store
        .read_current_summary(&world.worktree)
        .unwrap()
        .unwrap();
    summary.status = TestStatus::Running;
    summary.finished_at = None;
    store
        .write_summary(&current, &summary, world.caller.uid, world.caller.gid)
        .unwrap();
    // Reaper map removal follows event delivery; its stale handle must also be
    // treated as terminal, without rewriting a passing run to running.
    world.lifecycle.reconcile_orphans().unwrap();
    world.lifecycle.reconcile_orphans().unwrap();
    let status = world
        .lifecycle
        .status(world.worktree.to_str().unwrap(), &world.caller)
        .unwrap();
    assert_ne!(status.status, TestStatus::Running);
    assert_eq!(store.read_history(&world.worktree).unwrap().len(), 1);
}
