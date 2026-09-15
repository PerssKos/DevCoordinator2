use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use devcoordinator2_executor_core::{
    Cancellation, Executor, LocalPermitProvider, RunLogMetadata, artifact_receipts,
    protocol::{
        CaseSpec, CheckPhase, CheckPlan, CheckRole, CompletionMode, DiagnosticOrigin,
        DiagnosticReportFormat, DiagnosticReportSource, ErrorCategory, ExecutionPlan,
        FailureIndexEntry, FailureMode, LeafStatus, LogPhase, LogStream, ProofKind,
        RetainedArtifactSpec, RunStatus, Schema2, TerminationReason, ValidationTier,
    },
    source_digest,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Repository {
    root: PathBuf,
}

impl Repository {
    fn new(name: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "dc2-rust-executor-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create repository");
        run_git(&root, &["init", "-q"]);
        fs::write(root.join("README.md"), b"executor fixture\n").expect("fixture source");
        run_git(&root, &["add", "README.md"]);
        Self { root }
    }

    fn current(&self, run_id: &str) -> PathBuf {
        self.root.join(".devcoordinator").join(run_id)
    }

    fn logs(&self, run_id: &str) -> PathBuf {
        self.root
            .join(".devcoordinator/test/logs/runs")
            .join(run_id)
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove repository fixture");
    }
}

fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git command failed");
}

fn direct(name: &str, command: Vec<String>) -> CheckPlan {
    CheckPlan {
        display_name: None,
        source_name: None,
        expected_failure: None,
        expected_exit_code: None,
        name: name.into(),
        tier: ValidationTier::Development,
        role: CheckRole::Work,
        phase: CheckPhase::Check,
        resources: Vec::new(),
        after: Vec::new(),
        requires: Vec::new(),
        invalidates: Vec::new(),
        cwd: ".".into(),
        env: BTreeMap::new(),
        timeout_seconds: Some(5),
        completion: CompletionMode::Process,
        on_failure: FailureMode::Continue,
        produces: Vec::new(),
        consumes: Vec::new(),
        cacheable: false,
        cache_inputs: Vec::new(),
        fingerprint: String::new(),
        expect_failure: false,
        qualification_of: None,
        retained_artifacts: Vec::new(),
        diagnostic_sources: Vec::new(),
        command: Some(command),
        discover: None,
        case_command: None,
        cases: None,
    }
}

fn fixture(arguments: &[&str]) -> Vec<String> {
    std::iter::once(env!("CARGO_BIN_EXE_devcoordinator2-executor-test-fixture").to_owned())
        .chain(arguments.iter().map(|argument| (*argument).to_owned()))
        .collect()
}

fn fixture_exit(code: i32) -> Vec<String> {
    fixture(&["exit", &code.to_string()])
}

fn sha256(payload: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::new();
    for byte in Sha256::digest(payload) {
        write!(&mut result, "{byte:02x}").expect("hex digest");
    }
    result
}

fn dynamic_fanout(name: &str, manifest_kind: &str) -> CheckPlan {
    let mut fanout = direct(name, fixture_exit(0));
    fanout.discover = Some(fixture(&["manifest", manifest_kind]));
    fanout.command = None;
    fanout.case_command = Some(fixture_exit(0));
    fanout
}

fn plan(repository: &Repository, run_id: &str, checks: Vec<CheckPlan>) -> ExecutionPlan {
    fs::create_dir_all(repository.current(run_id)).expect("create run directory");
    fs::create_dir_all(repository.logs(run_id)).expect("create log directory");
    ExecutionPlan {
        database_checks: Default::default(),
        fixture_program: None,
        environment_files: BTreeMap::new(),
        schema: Schema2,
        run_id: run_id.into(),
        test: "complete".into(),
        worktree_root: repository.root.display().to_string(),
        current_dir: repository.current(run_id).display().to_string(),
        log_dir: repository.logs(run_id).display().to_string(),
        requested_tier: ValidationTier::Development,
        readiness_eligible: false,
        proof: ProofKind::Complete,
        selection: Vec::new(),
        origin_run_id: None,
        source_digest: source_digest(&repository.root).expect("source digest"),
        config_digest: "c".repeat(64),
        reused: BTreeMap::new(),
        reused_qualifications: BTreeSet::new(),
        case_selection: BTreeMap::new(),
        postgres_databases: BTreeMap::new(),
        checks,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_wait_is_visible_before_grant_and_excluded_from_process_duration() {
    use devcoordinator2_executor_core::{PermitProvider, PermitRequest};
    let repository = Repository::new("admission-progress");
    let provider = Arc::new(LocalPermitProvider::new(1).unwrap());
    let held = provider
        .acquire(PermitRequest {
            run_id: "held".into(),
            leaf_id: "held".into(),
        })
        .await
        .unwrap();
    let run_id = "run-admission-progress";
    let execution = Executor::new(
        plan(&repository, run_id, vec![direct("quick", fixture_exit(0))]),
        provider,
        Cancellation::default(),
    )
    .unwrap();
    let mut observations = execution.subscribe_progress();
    let running = tokio::spawn(execution.run());
    let queued = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            observations.changed().await.expect("executor progress");
            if let Some(report) = observations.borrow_and_update().as_ref()
                && report.checks[0]
                    .execution
                    .as_ref()
                    .is_some_and(|progress| progress.waiting == 1)
            {
                break report.clone();
            }
        }
    })
    .await
    .expect("waiting observation");
    assert!(queued.checks[0].started_at.is_none());
    assert_eq!(queued.checks[0].execution.as_ref().unwrap().executing, 0);
    assert_eq!(queued.capacity.capacity_wait_count, 1);
    assert!(
        !repository
            .logs(run_id)
            .join("checks/quick/check/stdout.log")
            .exists()
    );
    drop(held);
    let report = running.await.unwrap().unwrap();
    assert_eq!(report.status, RunStatus::Passed);
    let execution = report.checks[0].execution.as_ref().unwrap();
    assert_eq!(
        (
            execution.waiting,
            execution.admitted,
            execution.executing,
            execution.finished
        ),
        (0, 0, 0, 1)
    );
    assert!(execution.capacity_wait_ms <= (report.duration_seconds * 1000.0).ceil() as u64);
    assert!(execution.admitted_at_epoch_ms.unwrap() >= execution.queued_at_epoch_ms);
    assert!(execution.started_at_epoch_ms.unwrap() >= execution.admitted_at_epoch_ms.unwrap());
    assert_eq!(report.capacity.capacity_wait_count, 1);
    assert!(
        ((report.checks[0].duration_seconds.unwrap() * 1000.0) as u64)
            .abs_diff(execution.process_duration_ms)
            <= 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symlinked_platform_ancestor_resolves_before_containment_check() {
    let repository = Repository::new("symlink-ancestor");
    let alias = repository.root.with_extension("alias");
    symlink(&repository.root, &alias).expect("create worktree alias");
    let mut execution_plan = plan(
        &repository,
        "run-symlink-ancestor",
        vec![direct("check", fixture_exit(0))],
    );
    execution_plan.worktree_root = alias.display().to_string();
    execution_plan.current_dir = alias
        .join(".devcoordinator/run-symlink-ancestor")
        .display()
        .to_string();
    execution_plan.log_dir = alias
        .join(".devcoordinator/test/logs/runs/run-symlink-ancestor")
        .display()
        .to_string();
    let report = execute(execution_plan).await;
    assert_eq!(report.status, RunStatus::Passed);
    fs::remove_file(alias).expect("remove worktree alias");
}

async fn execute(plan: ExecutionPlan) -> devcoordinator2_executor_core::protocol::ExecutionReport {
    Executor::new(
        plan,
        Arc::new(LocalPermitProvider::unbounded()),
        Cancellation::default(),
    )
    .expect("executor")
    .run()
    .await
    .expect("run")
}

async fn execute_error(plan: ExecutionPlan) -> devcoordinator2_executor_core::ExecutorError {
    Executor::new(
        plan,
        Arc::new(LocalPermitProvider::unbounded()),
        Cancellation::default(),
    )
    .expect("executor")
    .run()
    .await
    .expect_err("execution should fail early")
}

fn run_metadata(repository: &Repository, run_id: &str) -> RunLogMetadata {
    let metadata: RunLogMetadata = serde_json::from_slice(
        &fs::read(repository.logs(run_id).join("run.json")).expect("run metadata"),
    )
    .expect("run metadata JSON");
    metadata.validate().expect("valid run metadata");
    metadata
}

fn status(
    report: &devcoordinator2_executor_core::protocol::ExecutionReport,
    name: &str,
) -> LeafStatus {
    report
        .checks
        .iter()
        .find(|check| check.name == name)
        .expect("check report")
        .status
}

fn failure<'a>(
    report: &'a devcoordinator2_executor_core::protocol::ExecutionReport,
    check: Option<&str>,
    case: Option<&str>,
) -> &'a FailureIndexEntry {
    report
        .failure_index
        .iter()
        .find(|entry| entry.check.as_deref() == check && entry.case.as_deref() == case)
        .expect("failure diagnostic")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_mismatch_leaves_terminal_incomplete_run_metadata() {
    let repository = Repository::new("source-mismatch-metadata");
    let mut execution_plan = plan(
        &repository,
        "run-source-mismatch-metadata",
        vec![direct("unit", fixture_exit(0))],
    );
    execution_plan.source_digest = "0".repeat(64);
    let _ = execute_error(execution_plan).await;
    let metadata = run_metadata(&repository, "run-source-mismatch-metadata");
    assert_eq!(metadata.status, RunStatus::Failed);
    assert!(!metadata.complete);
    assert!(metadata.finished_at_epoch_ms.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_current_directory_after_lease_is_catalogue_safe() {
    let repository = Repository::new("missing-current-metadata");
    let execution_plan = plan(
        &repository,
        "run-missing-current-metadata",
        vec![direct("unit", fixture_exit(0))],
    );
    fs::remove_dir_all(repository.current("run-missing-current-metadata"))
        .expect("remove disposable current directory");
    let _ = execute_error(execution_plan).await;
    let metadata = run_metadata(&repository, "run-missing-current-metadata");
    assert_eq!(metadata.status, RunStatus::Failed);
    assert!(!metadata.complete);
    assert!(metadata.finished_at_epoch_ms.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_preflight_invalidates_only_its_targets_and_siblings_finish() {
    let repository = Repository::new("preflight");
    let mut preflight = direct("preflight", fixture_exit(7));
    preflight.role = CheckRole::Preflight;
    preflight.invalidates = vec!["target".into()];
    let mut target = direct("target", fixture_exit(0));
    target.requires = vec!["preflight".into()];
    let independent = direct("independent", fixture_exit(0));
    let report = execute(plan(
        &repository,
        "run-preflight",
        vec![preflight, target, independent],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status(&report, "preflight"), LeafStatus::Failed);
    assert_eq!(status(&report, "target"), LeafStatus::Invalidated);
    assert_eq!(status(&report, "independent"), LeafStatus::Passed);
    let process = failure(&report, Some("preflight"), None);
    assert_eq!(process.error_category, ErrorCategory::ProcessExit);
    assert_eq!(process.exit.code, Some(7));
    let invalidated = failure(&report, Some("target"), None);
    assert_eq!(invalidated.error_category, ErrorCategory::Dependency);
    assert!(invalidated.log_refs.is_empty());
    let target_leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs("run-preflight")
                .join("checks/target/check/leaf.json"),
        )
        .expect("invalidated leaf metadata"),
    )
    .expect("invalidated leaf JSON");
    assert_eq!(target_leaf["process_started"], false);
    assert_eq!(target_leaf["complete"], true);
    assert!(
        !repository
            .logs("run-preflight")
            .join("checks/target/check/stdout.log")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transitive_failed_requirements_finish_in_reverse_order_without_stalling() {
    let repository = Repository::new("transitive-requirements");
    let first = direct("first", fixture_exit(7));
    let mut second = direct("second", fixture_exit(0));
    second.requires = vec!["first".into()];
    let mut third = direct("third", fixture_exit(0));
    third.requires = vec!["second".into()];
    let independent = direct("independent", fixture_exit(0));
    let report = execute(plan(
        &repository,
        "run-transitive",
        vec![third, second, first, independent],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status(&report, "first"), LeafStatus::Failed);
    assert_eq!(status(&report, "second"), LeafStatus::NotMeaningful);
    assert_eq!(status(&report, "third"), LeafStatus::NotMeaningful);
    assert_eq!(status(&report, "independent"), LeafStatus::Passed);
    assert!(report.checks.iter().all(|check| check.status.is_terminal()));
    report.validate().expect("terminal report");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reused_leaf_is_terminal_without_invented_process_streams() {
    let repository = Repository::new("reused-leaf");
    fs::write(repository.root.join("artifact.bin"), b"stable").expect("artifact");
    let mut check = direct("unit", fixture_exit(99));
    check.produces = vec!["artifact.bin".into()];
    let receipts = artifact_receipts(&repository.root, &check.produces).expect("receipts");
    let mut execution_plan = plan(&repository, "run-reused-leaf", vec![check]);
    execution_plan.reused.insert("unit".into(), receipts);
    let report = execute(execution_plan).await;
    assert_eq!(report.checks[0].status, LeafStatus::Reused);
    assert!(report.checks[0].streams.is_empty());
    let leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs("run-reused-leaf")
                .join("checks/unit/check/leaf.json"),
        )
        .expect("reused leaf metadata"),
    )
    .expect("reused leaf JSON");
    assert_eq!(leaf["process_started"], false);
    assert_eq!(leaf["complete"], true);
    assert!(
        !repository
            .logs("run-reused-leaf")
            .join("checks/unit/check/stdout.log")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_tier_runs_that_tier_and_every_lower_tier_only() {
    let repository = Repository::new("tiers");
    let development = direct("development", fixture_exit(0));
    let mut release = direct("release", fixture_exit(9));
    release.tier = ValidationTier::Release;
    let report = execute(plan(&repository, "run-tiers", vec![development, release])).await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(report.checks.len(), 1);
    assert_eq!(report.checks[0].name, "development");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_fanout_is_all_settled_and_case_reports_are_sorted() {
    let repository = Repository::new("fanout");
    let mut fanout = direct("cases", fixture_exit(0));
    fanout.command = None;
    fanout.case_command = Some(fixture(&["case-status"]));
    fanout.cases = Some(vec![
        CaseSpec {
            id: "z-pass".into(),
            args: vec!["good".into()],
            postgres: None,
        },
        CaseSpec {
            id: "a-fail".into(),
            args: vec!["bad".into()],
            postgres: None,
        },
    ]);
    let report = execute(plan(&repository, "run-fanout", vec![fanout])).await;
    let check = &report.checks[0];
    assert_eq!(check.status, LeafStatus::Failed);
    assert_eq!(check.cases[0].id, "a-fail");
    assert_eq!(check.cases[0].status, LeafStatus::Failed);
    assert_eq!(check.cases[1].id, "z-pass");
    assert_eq!(check.cases[1].status, LeafStatus::Passed);
    assert_eq!(check.case_count, 2);
    assert!(!check.cases_truncated);
    assert!(
        repository
            .current("run-fanout")
            .join("checks/cases/case-report.json")
            .is_file()
    );
    assert_eq!(report.failure_index[0].case.as_deref(), Some("a-fail"));
    let failed_case = failure(&report, Some("cases"), Some("a-fail"));
    assert_eq!(failed_case.error_category, ErrorCategory::ProcessExit);
    assert_eq!(failed_case.exit.code, Some(1));
    assert!(
        failed_case
            .log_refs
            .iter()
            .all(|reference| reference.phase == LogPhase::Case
                && reference.case.as_deref() == Some("a-fail"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_dynamic_manifest_fails_without_starting_cases() {
    let repository = Repository::new("manifest");
    let fanout = dynamic_fanout("cases", "oversized");
    let report = execute(plan(&repository, "run-manifest", vec![fanout])).await;
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    let diagnostic = failure(&report, Some("cases"), None);
    assert_eq!(
        diagnostic.error_category,
        ErrorCategory::StructuredEvidenceInvalid
    );
    assert!(
        diagnostic
            .log_refs
            .iter()
            .all(|reference| reference.phase == LogPhase::Discovery)
    );
    assert!(report.checks[0].cases.is_empty());
    let leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs("run-manifest")
                .join("checks/cases/discovery/leaf.json"),
        )
        .expect("discovery metadata"),
    )
    .expect("discovery metadata JSON");
    assert_eq!(leaf["status"], "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discovery_stdout_is_log_noise_and_manifest_comes_only_from_descriptor() {
    let repository = Repository::new("manifest-descriptor");
    let fanout = dynamic_fanout("cases", "one-noise");
    let report = execute(plan(&repository, "run-manifest-descriptor", vec![fanout])).await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(report.checks[0].case_count, 1);
    assert_eq!(report.checks[0].cases[0].id, "one");
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-manifest-descriptor")
                .join("checks/cases/discovery/stdout.log")
        )
        .expect("discovery stdout"),
        "ordinary discovery noise"
    );
    let saved = fs::read_to_string(
        repository
            .current("run-manifest-descriptor")
            .join("checks/cases/manifest.json"),
    )
    .expect("saved manifest");
    assert!(saved.starts_with("{\"schema\":2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_invalid_and_trailing_descriptor_manifests_are_rejected() {
    for name in ["missing", "invalid", "trailing"] {
        let repository = Repository::new(name);
        let report = execute(plan(
            &repository,
            &format!("run-{name}"),
            vec![dynamic_fanout("cases", name)],
        ))
        .await;
        assert_eq!(report.status, RunStatus::Failed, "{name}");
        assert_eq!(report.checks[0].status, LeafStatus::Failed, "{name}");
        assert!(report.checks[0].cases.is_empty(), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_manifest_cleanup_terminates_discovery_descendants() {
    let repository = Repository::new("manifest-cleanup");
    let socket = repository.root.join(".devcoordinator/descendant.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let mut fanout = dynamic_fanout("cases", "invalid");
    fanout.discover = Some(fixture(&[
        "invalid-manifest-with-child",
        socket.to_str().unwrap(),
    ]));
    let report = execute(plan(&repository, "run-manifest-cleanup", vec![fanout])).await;
    assert_eq!(report.status, RunStatus::Failed);
    let (mut descendant, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut observed = Vec::new();
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(
        Duration::from_secs(3),
        descendant.read_to_end(&mut observed),
    )
    .await
    .expect("descendant closed its socket after cleanup")
    .unwrap();
    assert_eq!(observed, b"ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaf_deadline_terminates_a_signal_resistant_process_group() {
    let repository = Repository::new("deadline");
    let mut slow = direct("slow", fixture(&["sleep-ignore-term"]));
    slow.timeout_seconds = Some(1);
    let report = execute(plan(&repository, "run-deadline", vec![slow])).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::TimedOut);
    assert!(report.duration_seconds < 5.0);
    let diagnostic = failure(&report, Some("slow"), None);
    assert_eq!(diagnostic.error_category, ErrorCategory::Timeout);
    assert_eq!(
        diagnostic.termination_reason,
        Some(TerminationReason::DeadlineExceeded)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_event_identity_is_exact() {
    let repository = Repository::new("event-identity");
    let mut service = direct("service", fixture(&["event", "wrong"]));
    service.completion = CompletionMode::Event;
    service.timeout_seconds = Some(3);
    let report = execute(plan(&repository, "run-event-wrong", vec![service])).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Unsafe);
    let diagnostic = failure(&report, Some("service"), None);
    assert_eq!(
        diagnostic.error_category,
        ErrorCategory::StructuredEvidenceInvalid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passed_event_keeps_service_alive_for_dependents_then_cleans_it_up() {
    let repository = Repository::new("event-service");
    let mut service = direct("service", fixture(&["event", "exact"]));
    service.completion = CompletionMode::Event;
    let mut dependent = direct("dependent", fixture_exit(0));
    dependent.requires = vec!["service".into()];
    let report = execute(plan(
        &repository,
        "run-event-pass",
        vec![service, dependent],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(status(&report, "service"), LeafStatus::Passed);
    assert_eq!(status(&report, "dependent"), LeafStatus::Passed);
    assert_eq!(report.checks[0].streams.len(), 2);
    let leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs("run-event-pass")
                .join("checks/service/check/leaf.json"),
        )
        .expect("service leaf metadata"),
    )
    .expect("service leaf JSON");
    assert_eq!(leaf["complete"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_ready_service_remains_executing_while_its_dependent_uses_it() {
    let repository = Repository::new("event-live-progress");
    let mut service = direct("service", fixture(&["event", "valid"]));
    service.completion = CompletionMode::Event;
    let release = repository.root.join(".devcoordinator/release.fifo");
    fs::create_dir_all(release.parent().unwrap()).unwrap();
    let encoded = std::ffi::CString::new(release.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the CString is live and this new path belongs to the disposable fixture.
    assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
    let mut dependent = direct(
        "dependent",
        fixture(&["wait-fifo", release.to_str().unwrap()]),
    );
    dependent.requires = vec!["service".into()];
    let run_id = "run-event-live-progress";
    let execution = Executor::new(
        plan(&repository, run_id, vec![service, dependent]),
        Arc::new(LocalPermitProvider::unbounded()),
        Cancellation::default(),
    )
    .unwrap();
    let mut observations = execution.subscribe_progress();
    let running = tokio::spawn(execution.run());
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            observations.changed().await.expect("executor progress");
            if let Some(report) = observations.borrow_and_update().as_ref()
                && report.checks[0].status == LeafStatus::Passed
                && report.checks[1]
                    .execution
                    .as_ref()
                    .is_some_and(|p| p.executing == 1)
            {
                break report.clone();
            }
        }
    })
    .await
    .expect("live service observation");
    assert_eq!(observed.checks[0].execution.as_ref().unwrap().executing, 1);
    assert_eq!(observed.checks[0].execution.as_ref().unwrap().finished, 0);
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .open(release)
            .unwrap()
            .write_all(b"1")
            .unwrap();
    })
    .await
    .unwrap();
    let complete = running.await.unwrap().unwrap();
    assert_eq!(complete.status, RunStatus::Passed);
    assert_eq!(complete.checks[0].execution.as_ref().unwrap().executing, 0);
    assert_eq!(complete.checks[0].execution.as_ref().unwrap().finished, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_larger_than_four_mibibytes_are_complete_and_hashed() {
    let repository = Repository::new("logs");
    let noisy = direct("noisy", fixture(&["repeat-stdout", "5242880"]));
    let report = execute(plan(&repository, "run-logs", vec![noisy])).await;
    let check = &report.checks[0];
    assert_eq!(check.status, LeafStatus::Passed);
    let stdout = check
        .streams
        .iter()
        .find(|stream| stream.log_ref.stream == LogStream::Stdout)
        .expect("stdout summary");
    assert_eq!(stdout.bytes, 5 * 1024 * 1024);
    assert_eq!(stdout.lines, 1);
    assert!(stdout.complete);
    assert_eq!(stdout.sha256, sha256(&vec![b'x'; 5 * 1024 * 1024]));
    assert_eq!(
        fs::metadata(
            repository
                .logs("run-logs")
                .join("checks/noisy/check/stdout.log")
        )
        .expect("stdout log")
        .len(),
        5 * 1024 * 1024
    );
    assert!(!repository.logs("run-logs").join("stdout.log").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_leaf_output_remains_separate_without_aggregate_streams() {
    let repository = Repository::new("aggregate-logs");
    let one = direct("one", fixture(&["streams", "one-out", "one-err"]));
    let two = direct("two", fixture(&["streams", "two-out", "two-err"]));
    let report = execute(plan(&repository, "run-aggregate-logs", vec![one, two])).await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-aggregate-logs")
                .join("checks/one/check/stdout.log")
        )
        .expect("one stdout"),
        "one-out\n"
    );
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-aggregate-logs")
                .join("checks/two/check/stderr.log")
        )
        .expect("two stderr"),
        "two-err\n"
    );
    assert!(
        !repository
            .logs("run-aggregate-logs")
            .join("stdout.log")
            .exists()
    );
    assert!(
        !repository
            .logs("run-aggregate-logs")
            .join("stderr.log")
            .exists()
    );
    assert!(report.checks.iter().all(|check| check.streams.len() == 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_cancellation_stops_running_leaf_and_finishes_report() {
    let repository = Repository::new("cancel");
    let slow = direct("slow", fixture(&["sleep", "30"]));
    let execution_plan = plan(&repository, "run-cancel", vec![slow]);
    let cancellation = Cancellation::default();
    let executor = Executor::new(
        execution_plan,
        Arc::new(LocalPermitProvider::unbounded()),
        cancellation.clone(),
    )
    .expect("executor");
    let mut observations = executor.subscribe_progress();
    let task = tokio::spawn(executor.run());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            observations.changed().await.expect("executor progress");
            if observations
                .borrow_and_update()
                .as_ref()
                .is_some_and(|report| {
                    report.checks[0]
                        .execution
                        .as_ref()
                        .is_some_and(|progress| progress.executing == 1)
                })
            {
                break;
            }
        }
    })
    .await
    .expect("running leaf observation");
    cancellation.cancel();
    let report = task.await.expect("join").expect("report");
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Cancelled);
    assert!(report.duration_seconds < 5.0);
    let diagnostic = failure(&report, Some("slow"), None);
    assert_eq!(diagnostic.error_category, ErrorCategory::Cancellation);
    assert_eq!(
        diagnostic.termination_reason,
        Some(TerminationReason::RunCancelled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_runs_after_failed_prerequisites_without_turning_the_run_green() {
    let repository = Repository::new("cleanup-failure");
    let mut failed = direct("work", fixture_exit(7));
    failed.on_failure = FailureMode::Stop;
    let mut cleanup = direct(
        "cleanup",
        fixture(&["write-relative", ".devcoordinator/cleaned", "yes"]),
    );
    cleanup.phase = CheckPhase::Cleanup;
    cleanup.requires = vec!["work".into()];
    let report = execute(plan(
        &repository,
        "run-cleanup-failure",
        vec![failed, cleanup],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    assert_eq!(report.checks[1].status, LeafStatus::Passed);
    assert!(!report.source_changed);
    assert_eq!(
        fs::read(repository.root.join(".devcoordinator/cleaned")).unwrap(),
        b"yes"
    );
    assert!(
        report
            .phase_durations
            .iter()
            .any(|phase| phase.phase == CheckPhase::Cleanup && phase.checks == 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_runs_when_cancellation_prevents_the_work_from_starting() {
    let repository = Repository::new("cleanup-cancelled");
    let work = direct("work", fixture(&["sleep", "30"]));
    let mut cleanup = direct(
        "cleanup",
        fixture(&["write-relative", ".devcoordinator/cleaned", "yes"]),
    );
    cleanup.phase = CheckPhase::Cleanup;
    cleanup.after = vec!["work".into()];
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let report = Executor::new(
        plan(&repository, "run-cleanup-cancel", vec![work, cleanup]),
        Arc::new(LocalPermitProvider::unbounded()),
        cancellation,
    )
    .unwrap()
    .run()
    .await
    .unwrap();
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Cancelled);
    assert_eq!(report.checks[1].status, LeafStatus::Passed);
    assert!(!report.source_changed);
    assert_eq!(
        fs::read(repository.root.join(".devcoordinator/cleaned")).unwrap(),
        b"yes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_reclaims_resources_after_event_service_consumers_finish() {
    use devcoordinator2_executor_core::protocol::{ResourceAccess, ResourceClaim, ResourceKind};
    let repository = Repository::new("cleanup-service-resource");
    let claim = ResourceClaim {
        kind: ResourceKind::Directory,
        id: ".devcoordinator/service".into(),
        access: ResourceAccess::Exclusive,
    };
    let mut service = direct("service", fixture(&["event", "valid"]));
    service.completion = CompletionMode::Event;
    service.resources = vec![claim.clone()];
    let mut work = direct("work", fixture_exit(0));
    work.requires = vec!["service".into()];
    let mut cleanup = direct("cleanup", fixture_exit(0));
    cleanup.phase = CheckPhase::Cleanup;
    cleanup.requires = vec!["work".into()];
    cleanup.resources = vec![claim];
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        execute(plan(
            &repository,
            "run-cleanup-service",
            vec![service, work, cleanup],
        )),
    )
    .await
    .expect("service resource released before cleanup");
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(report.checks[2].status, LeafStatus::Passed);
    assert_eq!(report.checks[0].execution.as_ref().unwrap().executing, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qualification_rejects_an_unrelated_nonzero_exit() {
    let repository = Repository::new("qualification-wrong-failure");
    let mut qualification = direct("qualification", fixture_exit(7));
    qualification.phase = CheckPhase::Qualification;
    qualification.role = CheckRole::Preflight;
    qualification.expect_failure = true;
    qualification.expected_failure = Some(format!("sha256:{}", "a".repeat(64)));
    qualification.qualification_of = Some("target".into());
    qualification.invalidates = vec!["target".into()];
    let target = direct("target", fixture_exit(0));
    let report = execute(plan(
        &repository,
        "run-qualification-wrong",
        vec![qualification, target],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    assert_eq!(report.checks[1].status, LeafStatus::Invalidated);
}

fn cache_repository(name: &str) -> Repository {
    let repository = Repository::new(name);
    fs::write(
        repository.root.join(".gitignore"),
        "artifact.bin\ndependency.txt\n",
    )
    .unwrap();
    run_git(&repository.root, &["add", ".gitignore"]);
    repository
}

fn cached_build() -> CheckPlan {
    let mut build = direct(
        "build",
        fixture(&[
            "counted-build",
            "README.md",
            "artifact.bin",
            ".devcoordinator/build-count",
        ]),
    );
    build.phase = CheckPhase::Build;
    build.cacheable = true;
    build.cache_inputs = vec!["README.md".into()];
    build.produces = vec!["artifact.bin".into()];
    build.fingerprint = "c".repeat(64);
    build
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_reuses_a_completed_build_and_invalidates_artifact_source_and_consumed_inputs() {
    let repository = cache_repository("cache-inputs");
    let build = cached_build();
    let failed = execute(plan(
        &repository,
        "run-cache-first",
        vec![build.clone(), direct("unrelated", fixture_exit(7))],
    ))
    .await;
    assert_eq!(failed.status, RunStatus::Failed);
    assert!(!failed.source_changed);
    let reused = execute(plan(&repository, "run-cache-second", vec![build.clone()])).await;
    assert_eq!(reused.checks[0].status, LeafStatus::Reused);
    assert_eq!(
        reused
            .phase_durations
            .iter()
            .find(|phase| phase.phase == CheckPhase::Build)
            .unwrap()
            .duration_seconds,
        0.0
    );
    assert_eq!(
        fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap(),
        "1"
    );
    fs::write(repository.root.join("artifact.bin"), b"corrupt output").unwrap();
    let repaired = execute(plan(&repository, "run-cache-output", vec![build.clone()])).await;
    assert_eq!(repaired.checks[0].status, LeafStatus::Passed);
    fs::write(repository.root.join("README.md"), b"changed input").unwrap();
    let changed = execute(plan(&repository, "run-cache-source", vec![build.clone()])).await;
    assert_eq!(changed.checks[0].status, LeafStatus::Passed);
    assert_eq!(
        fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap(),
        "3"
    );
    let mut dependent = build;
    dependent.requires = vec!["dependency".into()];
    dependent.consumes = vec![devcoordinator2_executor_core::protocol::ConsumedArtifact {
        check: "dependency".into(),
        path: "dependency.txt".into(),
    }];
    for (index, version, expected) in [
        (0, "one", LeafStatus::Passed),
        (1, "two", LeafStatus::Passed),
        (2, "two", LeafStatus::Reused),
    ] {
        let mut producer = direct(
            "dependency",
            fixture(&["write-relative", "dependency.txt", version]),
        );
        producer.produces = vec!["dependency.txt".into()];
        let report = execute(plan(
            &repository,
            &format!("run-cache-consumed-{index}"),
            vec![producer, dependent.clone()],
        ))
        .await;
        assert_eq!(report.status, RunStatus::Passed);
        assert_eq!(report.checks[1].status, expected);
    }
    assert_eq!(
        fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap(),
        "5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_never_publishes_evidence_from_a_run_whose_source_changed() {
    let repository = cache_repository("cache-seal");
    let build = cached_build();
    let mut mutation = direct(
        "mutation",
        fixture(&["write-relative", "README.md", "changed while running"]),
    );
    mutation.requires = vec!["build".into()];
    let invalid = execute(plan(
        &repository,
        "run-cache-invalid",
        vec![build.clone(), mutation],
    ))
    .await;
    assert!(invalid.source_changed);
    fs::write(repository.root.join("README.md"), b"executor fixture\n").unwrap();
    let next = execute(plan(&repository, "run-cache-after-invalid", vec![build])).await;
    assert_eq!(next.checks[0].status, LeafStatus::Passed);
    assert_eq!(
        fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap(),
        "2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumed_artifact_must_still_match_its_producer_receipt() {
    use devcoordinator2_executor_core::protocol::ConsumedArtifact;
    let repository = Repository::new("producer-receipt");
    let artifact = ".devcoordinator/producer.bin";
    let mut producer = direct(
        "producer",
        fixture(&["write-relative", artifact, "produced"]),
    );
    producer.produces = vec![artifact.into()];
    let mut replacement = direct(
        "replacement",
        fixture(&["write-relative", artifact, "replaced"]),
    );
    replacement.requires = vec!["producer".into()];
    let mut consumer = direct("consumer", fixture_exit(0));
    consumer.requires = vec!["producer".into(), "replacement".into()];
    consumer.consumes = vec![ConsumedArtifact {
        check: "producer".into(),
        path: artifact.into(),
    }];
    let report = execute(plan(
        &repository,
        "run-producer-receipt",
        vec![producer, replacement, consumer],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Passed);
    assert_eq!(report.checks[2].status, LeafStatus::Failed);
    assert!(
        report.checks[2].streams.is_empty(),
        "the consumer must not execute with replaced inputs"
    );
}

#[tokio::test]
async fn cancellation_interrupts_resource_wait_before_cache_or_process_execution() {
    use devcoordinator2_executor_core::protocol::{ResourceAccess, ResourceClaim, ResourceKind};
    use devcoordinator2_executor_core::{
        PermitFuture, PermitProvider, PermitRequest, ReservationFuture,
    };
    struct WaitingProvider {
        entered: tokio::sync::Notify,
        inner: LocalPermitProvider,
    }
    impl PermitProvider for WaitingProvider {
        fn acquire<'a>(&'a self, request: PermitRequest) -> PermitFuture<'a> {
            self.inner.acquire(request)
        }
        fn reserve<'a>(&'a self, _: &'a str, _: &'a str) -> ReservationFuture<'a> {
            Box::pin(async {
                self.entered.notify_one();
                std::future::pending().await
            })
        }
    }
    let provider = Arc::new(WaitingProvider {
        entered: tokio::sync::Notify::new(),
        inner: LocalPermitProvider::unbounded(),
    });
    let repository = cache_repository("resource-cancel");
    let mut check = cached_build();
    check.resources = vec![ResourceClaim {
        kind: ResourceKind::Directory,
        id: ".devcoordinator/build".into(),
        access: ResourceAccess::Exclusive,
    }];
    let cancellation = Cancellation::default();
    let executor = Executor::new(
        plan(&repository, "run-resource-cancel", vec![check]),
        provider.clone(),
        cancellation.clone(),
    )
    .unwrap();
    let running = tokio::spawn(executor.run());
    tokio::time::timeout(Duration::from_secs(3), provider.entered.notified())
        .await
        .unwrap();
    cancellation.cancel();
    let report = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.checks[0].status, LeafStatus::Cancelled);
    assert!(!report.checks[0].resource_waiting);
    assert!(!repository.root.join(".devcoordinator/build-count").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focused_reuse_checks_configuration_toolchain_and_corrupt_receipts() {
    let repository = cache_repository("focused-cache-inputs");
    fs::create_dir_all(repository.root.join(".devcoordinator")).unwrap();
    let executable = repository.root.join(".devcoordinator/compiler");
    let mut check = cached_build();
    fs::copy(&check.command.as_ref().unwrap()[0], &executable).unwrap();
    check.command.as_mut().unwrap()[0] = executable.to_string_lossy().into_owned();
    let first = execute(plan(&repository, "run-cache-origin", vec![check.clone()])).await;
    assert_eq!(first.checks[0].status, LeafStatus::Passed);
    let mut selected = plan(&repository, "run-cache-focused", vec![check.clone()]);
    selected.test = "focused-target".into();
    let reused = execute(selected).await;
    assert_eq!(reused.checks[0].status, LeafStatus::Reused);
    let mut configuration = plan(&repository, "run-cache-config", vec![check.clone()]);
    configuration.config_digest = "d".repeat(64);
    assert_eq!(
        execute(configuration).await.checks[0].status,
        LeafStatus::Passed
    );
    let receipt = repository
        .root
        .join(".devcoordinator/test/cache/build.json");
    fs::write(&receipt, b"corrupt receipt").unwrap();
    assert_eq!(
        execute(plan(&repository, "run-cache-corrupt", vec![check.clone()]))
            .await
            .checks[0]
            .status,
        LeafStatus::Passed
    );
    let before = fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap();
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&executable)
        .unwrap()
        .write_all(b"toolchain revision")
        .unwrap();
    assert_eq!(
        execute(plan(&repository, "run-cache-toolchain", vec![check]))
            .await
            .checks[0]
            .status,
        LeafStatus::Passed
    );
    let after = fs::read_to_string(repository.root.join(".devcoordinator/build-count")).unwrap();
    assert_eq!(
        after.parse::<u32>().unwrap(),
        before.parse::<u32>().unwrap() + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qualification_accepts_only_its_measured_failure_and_then_runs_the_corrected_case() {
    let repository = Repository::new("qualified-failure");
    let mut negative = direct("qualification", fixture(&["junit", "valid"]));
    negative.diagnostic_sources = vec![DiagnosticReportSource {
        format: DiagnosticReportFormat::Junit,
        path: "junit.xml".into(),
    }];
    let baseline = execute(plan(
        &repository,
        "run-negative-baseline",
        vec![negative.clone()],
    ))
    .await;
    let expected = baseline
        .failure_index
        .iter()
        .find(|failure| failure.origin == DiagnosticOrigin::Junit)
        .unwrap()
        .fingerprint
        .clone();
    assert_eq!(baseline.status, RunStatus::Failed);
    negative.name = "composed-qualification".into();
    negative.source_name = Some("qualification".into());
    negative.display_name = Some("parser / qualification".into());
    negative.phase = CheckPhase::Qualification;
    negative.role = CheckRole::Preflight;
    negative.expect_failure = true;
    negative.expected_failure = Some(expected);
    negative.expected_exit_code = Some(0);
    negative.qualification_of = Some("corrected".into());
    negative.invalidates = vec!["corrected".into()];
    let corrected = direct("corrected", fixture_exit(0));
    let report = execute(plan(
        &repository,
        "run-qualified",
        vec![negative.clone(), corrected.clone()],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Passed);
    assert!(
        report
            .checks
            .iter()
            .all(|check| check.status == LeafStatus::Passed)
    );
    assert_eq!(
        report.checks[0].display_name.as_deref(),
        Some("parser / qualification")
    );
    for defect in ["valid-then-crash", "unrelated-failure"] {
        negative.command = Some(fixture(&["junit", defect]));
        let failed = execute(plan(
            &repository,
            &format!("run-{defect}"),
            vec![negative.clone(), corrected.clone()],
        ))
        .await;
        assert_eq!(failed.status, RunStatus::Failed, "{defect}");
        assert_eq!(failed.checks[0].status, LeafStatus::Failed, "{defect}");
        assert_eq!(failed.checks[1].status, LeafStatus::Invalidated, "{defect}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_artifact_and_source_change_have_distinct_structured_categories() {
    let artifact_repository = Repository::new("artifact-diagnostic");
    let mut artifact = direct("artifact", fixture_exit(0));
    artifact.produces = vec!["missing.bin".into()];
    let artifact_report = execute(plan(
        &artifact_repository,
        "run-artifact-diagnostic",
        vec![artifact],
    ))
    .await;
    let artifact_failure = failure(&artifact_report, Some("artifact"), None);
    assert_eq!(artifact_failure.error_category, ErrorCategory::Artifact);
    assert_eq!(artifact_failure.exit.code, Some(0));
    let artifact_leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            artifact_repository
                .logs("run-artifact-diagnostic")
                .join("checks/artifact/check/leaf.json"),
        )
        .expect("artifact leaf metadata"),
    )
    .expect("artifact leaf JSON");
    assert_eq!(artifact_leaf["status"], "failed");

    let source_repository = Repository::new("source-diagnostic");
    let mutate = direct(
        "mutate",
        fixture(&["write-relative", "README.md", "changed"]),
    );
    let source_report = execute(plan(
        &source_repository,
        "run-source-diagnostic",
        vec![mutate],
    ))
    .await;
    assert!(source_report.source_changed);
    assert_eq!(
        failure(&source_report, None, None).error_category,
        ErrorCategory::SourceChanged
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_exit_is_structured_without_arbitrary_process_prose() {
    let repository = Repository::new("signal-diagnostic");
    let signalled = direct("signalled", fixture(&["kill-self"]));
    let report = execute(plan(&repository, "run-signal-diagnostic", vec![signalled])).await;
    let diagnostic = failure(&report, Some("signalled"), None);
    assert_eq!(diagnostic.error_category, ErrorCategory::ProcessExit);
    assert_eq!(diagnostic.exit.code, None);
    assert_eq!(diagnostic.exit.signal, Some(9));
    let encoded = serde_json::to_string(&report.failure_index).expect("diagnostic JSON");
    assert!(!encoded.contains("process exited"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_open_failure_is_unsafe_and_prevents_process_start() {
    let repository = Repository::new("log-storage-failure");
    let check = direct("unit", fixture(&["write-scratch", "started", "yes"]));
    let execution_plan = plan(&repository, "run-log-storage-failure", vec![check]);
    let leaf_dir = repository
        .logs("run-log-storage-failure")
        .join("checks/unit/check");
    fs::create_dir_all(&leaf_dir).expect("leaf directory");
    fs::write(leaf_dir.join("stdout.log"), b"occupied").expect("occupied stream");
    let report = execute(execution_plan).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Unsafe);
    assert_eq!(
        failure(&report, Some("unit"), None).error_category,
        ErrorCategory::LogStorage
    );
    assert!(
        !repository
            .current("run-log-storage-failure")
            .join("scratch/unit/started")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_spawn_failure_is_internal_with_truthful_empty_streams() {
    let repository = Repository::new("spawn-failure");
    let report = execute(plan(
        &repository,
        "run-spawn-failure",
        vec![direct(
            "unit",
            vec!["/definitely/missing/devcoordinator-command".into()],
        )],
    ))
    .await;
    assert_eq!(
        report.checks[0].status,
        LeafStatus::Failed,
        "{:?}; {:?}",
        report.failure_index,
        report.checks[0].streams
    );
    assert_eq!(report.checks[0].streams.len(), 2);
    assert!(
        report.checks[0]
            .streams
            .iter()
            .all(|stream| { stream.bytes == 0 && stream.lines == 0 && stream.complete })
    );
    let diagnostic = failure(&report, Some("unit"), None);
    assert_eq!(diagnostic.error_category, ErrorCategory::Internal);
    let leaf: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs("run-spawn-failure")
                .join("checks/unit/check/leaf.json"),
        )
        .expect("spawn failure leaf metadata"),
    )
    .expect("spawn failure leaf JSON");
    assert_eq!(leaf["process_started"], false);
    assert_eq!(leaf["complete"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_metadata_write_failure_stops_the_live_process_as_unsafe() {
    let repository = Repository::new("log-write-failure");
    let started = std::time::Instant::now();
    let report = execute(plan(
        &repository,
        "run-log-write-failure",
        vec![direct("unit", fixture(&["break-log-storage"]))],
    ))
    .await;
    let leaf_dir = repository
        .logs("run-log-write-failure")
        .join("checks/unit/check");
    fs::set_permissions(&leaf_dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore leaf permissions");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(report.checks[0].status, LeafStatus::Unsafe);
    assert!(report.failure_index.iter().any(|entry| {
        entry.check.as_deref() == Some("unit") && entry.error_category == ErrorCategory::LogStorage
    }));
    assert_eq!(
        fs::read(leaf_dir.join("stdout.log")).expect("partial complete stream bytes"),
        b"stored-before-failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn governed_leaf_exposes_a_private_retained_evidence_directory() {
    let repository = Repository::new("visual-evidence-directory");
    let report = execute(plan(
        &repository,
        "run-visual-evidence-directory",
        vec![direct("formal-ui", fixture(&["write-evidence"]))],
    ))
    .await;
    assert_eq!(report.checks[0].status, LeafStatus::Passed);
    let evidence = repository
        .logs("run-visual-evidence-directory")
        .join("checks/formal-ui/check/evidence/journey-evidence.json");
    assert_eq!(
        fs::read_to_string(evidence).expect("retained journey evidence"),
        "{\"kind\":\"fixture\"}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_direct_check_snapshots_declared_retained_artifact_tree() {
    let repository = Repository::new("retained-artifact-tree");
    fs::write(repository.root.join(".gitignore"), b"browser-evidence/\n")
        .expect("ignore generated evidence");
    run_git(&repository.root, &["add", ".gitignore"]);
    let mut check = direct("browser", fixture(&["write-artifact-tree"]));
    check.retained_artifacts = vec![RetainedArtifactSpec {
        name: "production".into(),
        path: "browser-evidence".into(),
        max_bytes: 1024,
    }];
    let report = execute(plan(&repository, "run-retained-artifact-tree", vec![check])).await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(report.checks[0].retained_artifacts.len(), 1);
    let receipt = &report.checks[0].retained_artifacts[0];
    assert_eq!(receipt.name, "production");
    assert_eq!(receipt.files, 2);
    let retained = repository
        .logs("run-retained-artifact-tree")
        .join("checks/browser/check/evidence/retained/production");
    assert_eq!(
        fs::read_to_string(retained.join("result.json")).expect("result"),
        "{\"ok\":true}\n"
    );
    assert_eq!(
        fs::read(retained.join("nested/capture.png")).expect("capture"),
        b"png"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_direct_check_retains_diagnostics_without_reusable_build_receipts() {
    let repository = Repository::new("failed-retained-artifact-tree");
    fs::write(repository.root.join(".gitignore"), b"browser-evidence/\n")
        .expect("ignore generated evidence");
    run_git(&repository.root, &["add", ".gitignore"]);
    let mut check = direct("browser", fixture(&["write-failed-artifact-tree"]));
    check.produces = vec!["browser-evidence/result.json".into()];
    check.retained_artifacts = vec![RetainedArtifactSpec {
        name: "diagnostics".into(),
        path: "browser-evidence".into(),
        max_bytes: 1024,
    }];
    let report = execute(plan(
        &repository,
        "run-failed-retained-artifact-tree",
        vec![check],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    assert_eq!(report.checks[0].exit.code, Some(7));
    assert!(report.checks[0].artifacts.is_empty());
    assert_eq!(report.checks[0].retained_artifacts.len(), 1);
    assert_eq!(report.checks[0].retained_artifacts[0].files, 2);
    let retained = repository
        .logs("run-failed-retained-artifact-tree")
        .join("checks/browser/check/evidence/retained/diagnostics");
    assert_eq!(
        fs::read(retained.join("nested/capture.png")).expect("capture"),
        b"png"
    );
    assert_eq!(failure(&report, Some("browser"), None).exit.code, Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_discovery_and_case_logs_use_distinct_stable_leaves() {
    let repository = Repository::new("phase-layout");
    let mut fanout = dynamic_fanout("cases", "one-discovery");
    fanout.case_command = Some(fixture(&["print", "case"]));
    let direct_check = direct("direct", fixture(&["print", "direct"]));
    let report = execute(plan(
        &repository,
        "run-phase-layout",
        vec![direct_check, fanout],
    ))
    .await;
    assert_eq!(report.status, RunStatus::Passed);
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-phase-layout")
                .join("checks/direct/check/stdout.log")
        )
        .expect("direct log"),
        "direct\n"
    );
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-phase-layout")
                .join("checks/cases/discovery/stdout.log")
        )
        .expect("discovery log"),
        "discovery"
    );
    assert_eq!(
        fs::read_to_string(
            repository
                .logs("run-phase-layout")
                .join("checks/cases/cases/one/stdout.log")
        )
        .expect("case log"),
        "case\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_diagnostic_event_is_parsed_without_console_scraping() {
    let repository = Repository::new("diagnostic-event");
    let report = execute(plan(
        &repository,
        "run-diagnostic-event",
        vec![direct("unit", fixture(&["diagnostic-event"]))],
    ))
    .await;
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    let diagnostic = report
        .failure_index
        .iter()
        .find(|entry| entry.origin == DiagnosticOrigin::ExplicitEvent)
        .expect("explicit diagnostic");
    assert_eq!(diagnostic.error_category, ErrorCategory::Assertion);
    assert_eq!(diagnostic.exit.code, Some(7));
    assert_eq!(
        diagnostic.source.as_ref().map(|source| source.line),
        Some(81)
    );
    assert!(
        diagnostic
            .log_refs
            .iter()
            .all(|reference| reference.phase == LogPhase::Check)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_junit_source_is_confined_to_the_leaf_diagnostics_directory() {
    let repository = Repository::new("declared-junit");
    let mut check = direct("unit", fixture(&["junit", "valid"]));
    check.diagnostic_sources = vec![DiagnosticReportSource {
        format: DiagnosticReportFormat::Junit,
        path: "junit.xml".into(),
    }];
    let report = execute(plan(&repository, "run-declared-junit", vec![check])).await;
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    let diagnostic = report
        .failure_index
        .iter()
        .find(|entry| entry.origin == DiagnosticOrigin::Junit)
        .expect("JUnit diagnostic");
    assert_eq!(diagnostic.error_category, ErrorCategory::Assertion);
    assert_eq!(
        diagnostic.source.as_ref().map(|source| source.line),
        Some(17)
    );
    assert_eq!(
        diagnostic
            .expected
            .as_ref()
            .and_then(|value| value.preview.as_deref()),
        Some("ready")
    );
    assert!(
        repository
            .logs("run-declared-junit")
            .join("checks/unit/check/diagnostics.json")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_declared_report_fails_without_unbounded_allocation() {
    let repository = Repository::new("oversized-diagnostic-report");
    let mut check = direct("unit", fixture(&["junit", "oversized"]));
    check.diagnostic_sources = vec![DiagnosticReportSource {
        format: DiagnosticReportFormat::Junit,
        path: "junit.xml".into(),
    }];
    let report = execute(plan(
        &repository,
        "run-oversized-diagnostic-report",
        vec![check],
    ))
    .await;
    assert_eq!(report.checks[0].status, LeafStatus::Unsafe);
    assert!(report.failure_index.iter().any(|entry| {
        entry.error_category == ErrorCategory::StructuredEvidenceInvalid
            && entry.check.as_deref() == Some("unit")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_symlink_and_fifo_diagnostic_reports_are_rejected_without_blocking() {
    for name in ["symlink", "fifo"] {
        let repository = Repository::new(name);
        let mut check = direct("unit", fixture(&["junit", name]));
        check.diagnostic_sources = vec![DiagnosticReportSource {
            format: DiagnosticReportFormat::Junit,
            path: "junit.xml".into(),
        }];
        let started = std::time::Instant::now();
        let report = execute(plan(
            &repository,
            &format!("run-diagnostic-{name}"),
            vec![check],
        ))
        .await;
        assert!(started.elapsed() < Duration::from_secs(5), "{name}");
        assert_eq!(report.checks[0].status, LeafStatus::Unsafe, "{name}");
        assert!(report.failure_index.iter().any(|entry| {
            entry.error_category == ErrorCategory::StructuredEvidenceInvalid
                && entry.check.as_deref() == Some("unit")
        }));
    }
}
