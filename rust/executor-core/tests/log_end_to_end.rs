use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use devcoordinator2_executor_core::{
    Cancellation, Executor, LocalPermitProvider,
    log_query::{
        LogPruneRequest, LogQueryOperation, LogQueryOptions, LogQueryRequest, LogQueryResult,
        LogQuerySelector, execute_log_query, prune_logs,
    },
    protocol::{
        CaseSpec, CheckPhase, CheckPlan, CheckRole, CompletionMode, DiagnosticOrigin,
        DiagnosticReportFormat, DiagnosticReportSource, ErrorCategory, ExecutionPlan, FailureMode,
        LeafStatus, LogPhase, LogStream, ProofKind, RunStatus, Schema2, ValidationTier,
    },
    source_digest,
};

const RUN_ID: &str = "logs-20260902T120000Z-123-abcdef";
const REPOSITORY_ID: &str = "r0000000000000001";
const SENTINEL: &[u8] = b"FINAL-SENTINEL\n";
const FORMER_STREAM_LIMIT: usize = 4 * 1024 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Repository {
    root: PathBuf,
}

impl Repository {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "dc2-log-end-to-end-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create repository fixture");
        run_git(&root, &["init", "-q"]);
        fs::write(root.join("README.md"), b"progressive log fixture\n")
            .expect("write fixture source");
        run_git(&root, &["add", "README.md"]);
        Self { root }
    }

    fn current(&self) -> PathBuf {
        self.root.join(".devcoordinator").join(RUN_ID)
    }

    fn logs(&self) -> PathBuf {
        self.root
            .join(".devcoordinator/test/logs/runs")
            .join(RUN_ID)
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

fn fixture(arguments: &[&str]) -> Vec<String> {
    std::iter::once(env!("CARGO_BIN_EXE_devcoordinator2-executor-test-fixture").to_owned())
        .chain(arguments.iter().map(|argument| (*argument).to_owned()))
        .collect()
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
        timeout_seconds: Some(15),
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

fn query(
    operation: LogQueryOperation,
    check: Option<&str>,
    phase: Option<LogPhase>,
    case_id: Option<&str>,
    stream: Option<LogStream>,
    options: LogQueryOptions,
) -> LogQueryRequest {
    LogQueryRequest {
        schema: 2,
        operation,
        repository_id: REPOSITORY_ID.into(),
        selector: LogQuerySelector {
            run_id: Some(RUN_ID.into()),
            check: check.map(str::to_owned),
            phase,
            case_id: case_id.map(str::to_owned),
            stream,
        },
        options,
    }
}

fn sha256(payload: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut result = String::new();
    for byte in Sha256::digest(payload) {
        write!(&mut result, "{byte:02x}").expect("format digest");
    }
    result
}

fn check_status(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundreds_of_discovered_cases_finish_with_complete_queryable_logs() {
    let repository = Repository::new();
    fs::create_dir_all(repository.current()).expect("create run directory");
    fs::create_dir_all(repository.logs()).expect("create log directory");
    let mut check = direct("python-tests", fixture(&["exit", "0"]));
    check.command = None;
    check.discover = Some(fixture(&["manifest", "431-cases"]));
    check.case_command = Some(fixture(&["case-status"]));
    let plan = ExecutionPlan {
        database_checks: Default::default(),
        fixture_program: None,
        environment_files: BTreeMap::new(),
        schema: Schema2,
        run_id: RUN_ID.into(),
        test: "fanout-reproduction".into(),
        worktree_root: repository.root.display().to_string(),
        current_dir: repository.current().display().to_string(),
        log_dir: repository.logs().display().to_string(),
        requested_tier: ValidationTier::Development,
        readiness_eligible: false,
        proof: ProofKind::Complete,
        selection: Vec::new(),
        origin_run_id: None,
        source_digest: source_digest(&repository.root).expect("source digest"),
        config_digest: "c".repeat(64),
        reused: BTreeMap::new(),
        case_selection: BTreeMap::new(),
        postgres_databases: BTreeMap::new(),
        reused_qualifications: Default::default(),
        checks: vec![check],
    };
    let report = Executor::new(
        plan,
        Arc::new(LocalPermitProvider::new(64).expect("fixture capacity")),
        Cancellation::default(),
    )
    .expect("executor")
    .run()
    .await
    .expect("run");
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(report.checks[0].status, LeafStatus::Failed);
    assert_eq!(report.checks[0].case_count, 431);
    assert_eq!(
        report.checks[0]
            .cases
            .iter()
            .filter(|case| case.status == LeafStatus::Failed)
            .count(),
        6
    );
    assert!(report.checks[0].cases_truncated);
    assert!(!report.failure_index_truncated);
    for index in 0..431 {
        let path = repository.logs().join(format!(
            "checks/python-tests/cases/case-{index:04}/leaf.json"
        ));
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("case metadata"))
                .expect("valid metadata");
        assert_eq!(
            metadata["status"],
            if index < 6 { "failed" } else { "passed" }
        );
        assert_eq!(metadata["complete"], true);
    }
    let mut cursor = None;
    let mut rows = 0;
    loop {
        let result = execute_log_query(
            &repository.root,
            query(
                LogQueryOperation::Catalog,
                Some("python-tests"),
                None,
                None,
                None,
                LogQueryOptions {
                    cursor,
                    limit: Some(100),
                    ..LogQueryOptions::default()
                },
            ),
        )
        .expect("catalogue remains available");
        let LogQueryResult::Catalog(catalogue) = result else {
            panic!("catalogue result");
        };
        assert!(
            catalogue
                .entries
                .iter()
                .all(|entry| entry.complete && !entry.truncated)
        );
        rows += catalogue.entries.len();
        cursor = catalogue.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(rows, 864);
}

// UIL-TESTING-LOGS-001: exercise one real plan from execution through every
// progressive-disclosure query, including a pre-start leaf with no streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn executor_logs_remain_complete_queryable_and_catalogue_safe() {
    let repository = Repository::new();
    fs::create_dir_all(repository.current()).expect("create run directory");
    fs::create_dir_all(repository.logs()).expect("create log directory");

    let mut preflight = direct(
        "source-preflight",
        fixture(&["stderr-exit", "RAW-PREFLIGHT-DETAIL", "7"]),
    );
    preflight.role = CheckRole::Preflight;
    preflight.invalidates = vec!["never-started".into()];

    let mut never_started = direct(
        "never-started",
        fixture(&["write-relative", "NEVER_STARTED", "incorrectly started"]),
    );
    never_started.requires = vec!["source-preflight".into()];

    let noisy = direct(
        "large-output",
        fixture(&["large-output-sentinel", "5242880"]),
    );

    let mut structured = direct("structured-cases", fixture(&["exit", "0"]));
    structured.command = None;
    structured.case_command = Some(fixture(&["junit", "log-case"]));
    structured.cases = Some(vec![CaseSpec {
        id: "case-alpha".into(),
        args: Vec::new(),
        postgres: None,
    }]);
    structured.diagnostic_sources = vec![DiagnosticReportSource {
        format: DiagnosticReportFormat::Junit,
        path: "junit.xml".into(),
    }];

    let plan = ExecutionPlan {
        database_checks: Default::default(),
        fixture_program: None,
        environment_files: BTreeMap::new(),
        schema: Schema2,
        run_id: RUN_ID.into(),
        test: "progressive-logs".into(),
        worktree_root: repository.root.display().to_string(),
        current_dir: repository.current().display().to_string(),
        log_dir: repository.logs().display().to_string(),
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
        checks: vec![preflight, never_started, noisy, structured],
    };
    plan.validate().expect("strict schema-2 plan");

    let report = Executor::new(
        plan,
        Arc::new(LocalPermitProvider::unbounded()),
        Cancellation::default(),
    )
    .expect("executor")
    .run()
    .await
    .expect("execute plan");

    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        check_status(&report, "source-preflight"),
        LeafStatus::Failed
    );
    assert_eq!(
        check_status(&report, "never-started"),
        LeafStatus::Invalidated
    );
    assert_eq!(check_status(&report, "large-output"), LeafStatus::Passed);
    assert_eq!(
        check_status(&report, "structured-cases"),
        LeafStatus::Failed
    );
    assert!(!repository.root.join("NEVER_STARTED").exists());

    let invalidated = report
        .checks
        .iter()
        .find(|check| check.name == "never-started")
        .expect("invalidated report");
    assert!(invalidated.streams.is_empty());
    let invalidated_metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(
            repository
                .logs()
                .join("checks/never-started/check/leaf.json"),
        )
        .expect("invalidated leaf metadata"),
    )
    .expect("decode invalidated metadata");
    assert_eq!(invalidated_metadata["process_started"], false);
    assert_eq!(invalidated_metadata["status"], "invalidated");

    let mut expected = vec![b'x'; 5 * 1024 * 1024];
    expected.push(b'\n');
    expected.extend_from_slice(SENTINEL);
    assert!(expected.len() > FORMER_STREAM_LIMIT);
    let stored = fs::read(
        repository
            .logs()
            .join("checks/large-output/check/stdout.log"),
    )
    .expect("complete stdout");
    assert_eq!(stored, expected);

    assert!(
        repository
            .logs()
            .join("checks/structured-cases/cases/case-alpha/diagnostics/junit.xml")
            .is_file()
    );
    assert!(
        repository
            .logs()
            .join("checks/structured-cases/cases/case-alpha/diagnostics.json")
            .is_file()
    );
    assert!(!repository.logs().join("stdout.log").exists());
    assert!(!repository.logs().join("stderr.log").exists());
    assert!(!repository.logs().join("executor").exists());

    let ordinary_result = serde_json::to_string(&report).expect("encode completion");
    assert!(!ordinary_result.contains("FINAL-SENTINEL"));
    assert!(!ordinary_result.contains("RAW-PREFLIGHT-DETAIL"));
    assert!(!ordinary_result.contains("PRIVATE-JUNIT-PROSE"));

    let catalogue = match execute_log_query(
        &repository.root,
        query(
            LogQueryOperation::Catalog,
            None,
            None,
            None,
            None,
            LogQueryOptions {
                limit: Some(100),
                ..LogQueryOptions::default()
            },
        ),
    )
    .expect("catalogue must accept a terminal leaf with zero streams")
    {
        LogQueryResult::Catalog(result) => result,
        _ => panic!("expected catalogue result"),
    };
    assert_eq!(catalogue.entries.len(), 6);
    assert!(catalogue.next_cursor.is_none());
    assert!(
        catalogue
            .entries
            .iter()
            .all(|entry| entry.log_ref.phase != LogPhase::Executor)
    );
    assert!(catalogue.entries.iter().all(|entry| {
        entry.log_ref.run_id == RUN_ID
            && entry.log_ref.validate().is_ok()
            && !entry.truncated
            && entry.complete
    }));
    assert!(
        catalogue
            .entries
            .iter()
            .all(|entry| { entry.log_ref.check.as_deref() != Some("never-started") })
    );

    let large_stdout = catalogue
        .entries
        .iter()
        .find(|entry| {
            entry.log_ref.check.as_deref() == Some("large-output")
                && entry.log_ref.phase == LogPhase::Check
                && entry.log_ref.stream == LogStream::Stdout
        })
        .expect("large stdout catalogue row");
    assert_eq!(large_stdout.bytes, expected.len() as u64);
    assert_eq!(large_stdout.lines, Some(2));
    assert_eq!(
        large_stdout.sha256.as_deref(),
        Some(sha256(&expected).as_str())
    );
    assert!(large_stdout.expires_at.is_some());
    assert_eq!(large_stdout.depth_rank, Some(1));

    let structured_stdout = catalogue
        .entries
        .iter()
        .find(|entry| {
            entry.log_ref.check.as_deref() == Some("structured-cases")
                && entry.log_ref.phase == LogPhase::Case
                && entry.log_ref.case.as_deref() == Some("case-alpha")
                && entry.log_ref.stream == LogStream::Stdout
        })
        .expect("structured case catalogue row");
    assert!(structured_stdout.structured_evidence.available);
    assert!(
        structured_stdout
            .structured_evidence
            .formats
            .contains(&DiagnosticReportFormat::Junit)
    );

    let tail = match execute_log_query(
        &repository.root,
        query(
            LogQueryOperation::Tail,
            Some("large-output"),
            Some(LogPhase::Check),
            None,
            Some(LogStream::Stdout),
            LogQueryOptions {
                lines: Some(1),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        ),
    )
    .expect("tail")
    {
        LogQueryResult::Content(result) => result,
        _ => panic!("expected tail result"),
    };
    assert_eq!(tail.snapshot_bytes, expected.len() as u64);
    assert_eq!(tail.snapshot_lines, 2);
    assert_eq!(tail.segments.len(), 1);
    assert_eq!(tail.segments[0].line_start, 2);
    assert_eq!(tail.segments[0].text.as_deref(), Some("FINAL-SENTINEL\n"));
    assert_eq!(tail.segments[0].byte_end, expected.len() as u64);

    let search = match execute_log_query(
        &repository.root,
        query(
            LogQueryOperation::Search,
            Some("large-output"),
            Some(LogPhase::Check),
            None,
            Some(LogStream::Stdout),
            LogQueryOptions {
                text: Some("FINAL-SENTINEL".into()),
                max_matches: Some(5),
                context_lines: Some(0),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        ),
    )
    .expect("fixed-string search")
    {
        LogQueryResult::Search(result) => result,
        _ => panic!("expected search result"),
    };
    assert_eq!(search.matches.len(), 1);
    assert_eq!(search.matches[0].line_start, 2);
    assert_eq!(search.matches[0].line_end, 2);
    assert_eq!(search.matches[0].text.as_deref(), Some("FINAL-SENTINEL\n"));

    let sentinel_start = (expected.len() - SENTINEL.len()) as u64;
    let range = match execute_log_query(
        &repository.root,
        query(
            LogQueryOperation::Range,
            Some("large-output"),
            Some(LogPhase::Check),
            None,
            Some(LogStream::Stdout),
            LogQueryOptions {
                line_start: Some(2),
                line_end: Some(2),
                max_bytes: Some(1024),
                ..LogQueryOptions::default()
            },
        ),
    )
    .expect("exact line range")
    {
        LogQueryResult::Content(result) => result,
        _ => panic!("expected range result"),
    };
    assert_eq!(range.segments.len(), 1);
    assert_eq!(range.segments[0].byte_start, sentinel_start);
    assert_eq!(range.segments[0].byte_end, expected.len() as u64);
    assert_eq!(range.segments[0].text.as_deref(), Some("FINAL-SENTINEL\n"));

    let failure_context = match execute_log_query(
        &repository.root,
        query(
            LogQueryOperation::FailureContext,
            Some("structured-cases"),
            Some(LogPhase::Case),
            Some("case-alpha"),
            Some(LogStream::Stdout),
            LogQueryOptions {
                limit: Some(20),
                context_lines: Some(1),
                max_bytes: Some(4096),
                ..LogQueryOptions::default()
            },
        ),
    )
    .expect("structured failure context")
    {
        LogQueryResult::FailureContext(result) => result,
        _ => panic!("expected failure-context result"),
    };
    let junit = failure_context
        .failures
        .iter()
        .find(|failure| failure.origin == DiagnosticOrigin::Junit)
        .expect("JUnit failure");
    assert_eq!(junit.check.as_deref(), Some("structured-cases"));
    assert_eq!(
        junit.case.as_deref(),
        Some("case-alpha :: Parser::rejects-final-state")
    );
    assert_eq!(junit.error_category, ErrorCategory::Assertion);
    assert_eq!(junit.source.as_ref().map(|source| source.line), Some(37));
    assert_eq!(
        junit
            .expected
            .as_ref()
            .and_then(|value| value.preview.as_deref()),
        Some("ready")
    );
    assert_eq!(
        junit
            .actual
            .as_ref()
            .and_then(|value| value.preview.as_deref()),
        Some("pending")
    );
    assert_eq!(junit.occurrences, 2);
    assert!(junit.fingerprint.starts_with("sha256:"));
    assert_eq!(junit.log_refs.len(), 2);
    assert!(junit.log_refs.iter().all(|reference| {
        reference.run_id == RUN_ID
            && reference.check.as_deref() == Some("structured-cases")
            && reference.phase == LogPhase::Case
            && reference.case.as_deref() == Some("case-alpha")
            && reference.validate().is_ok()
    }));

    let prune = prune_logs(
        &repository.root,
        LogPruneRequest {
            schema: 2,
            repository_id: REPOSITORY_ID.into(),
            max_age_seconds: 24 * 60 * 60,
            case_depth: 3,
            active_run_id: None,
        },
    )
    .expect("deterministic no-op retention pass");
    assert_eq!(prune.removed_leaf_folders, 0);
    assert_eq!(prune.retained_active, 0);
}
