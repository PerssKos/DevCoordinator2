//! Strict Rust-native six-skill validation planning and execution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use devcoordinator2_executor_protocol::{
    CheckPlan, CheckRole, CompletionMode, ExecutionPlan, ExecutionReport, FailureMode, LeafStatus,
    ProofKind, RunStatus, Schema2, ValidationTier,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::audit_ledger::{
    create_directory_all_nofollow, read_bytes_nofollow, validate_directory_nofollow,
    write_new_bytes_nofollow,
};

pub const SKILL_NAMES: [&str; 6] = [
    "dev-coordinator",
    "formal-web-ui-verification",
    "full-repo-audit",
    "full-repo-test-coverage-audit",
    "ui-implementation-audit",
    "user-journey-docs-audit",
];

fn read_text(root: &Path, relative: &str) -> Result<String, String> {
    let path = root.join(relative);
    let bytes = read_bytes_nofollow(&path, Some(root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("required file is missing: {relative}"))?;
    String::from_utf8(bytes).map_err(|_| format!("required file is not UTF-8: {relative}"))
}

pub fn check_repository_layout(root: &Path) -> Result<(), String> {
    let skills =
        validate_directory_nofollow(&root.join("skills")).map_err(|error| error.to_string())?;
    let actual = std::fs::read_dir(&skills)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                && entry.path().join("SKILL.md").is_file()
        })
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let expected = SKILL_NAMES
        .map(str::to_owned)
        .into_iter()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "canonical skill set mismatch; missing={:?}, unexpected={:?}",
            expected.difference(&actual).collect::<Vec<_>>(),
            actual.difference(&expected).collect::<Vec<_>>()
        ));
    }
    for skill in SKILL_NAMES {
        for relative in [
            format!("skills/{skill}/SKILL.md"),
            format!("skills/{skill}/README.md"),
            format!("skills/{skill}/agents/openai.yaml"),
        ] {
            read_text(root, &relative)?;
        }
    }
    Ok(())
}

pub fn check_canonical_harness(root: &Path) -> Result<(), String> {
    for relative in [
        "rust/tooling/src/audit_common.rs",
        "rust/tooling/src/audit_evidence.rs",
        "rust/tooling/src/audit_findings.rs",
        "rust/tooling/src/audit_ledger.rs",
        "rust/tooling/src/audit_queue.rs",
        "rust/tooling/src/audit_targets.rs",
        "rust/tooling/src/audit_verify.rs",
    ] {
        read_text(root, relative)?;
    }
    if root.join("full_repo_harness").exists() {
        return Err("retired Python full_repo_harness still exists".to_owned());
    }
    for skill in [
        "full-repo-audit",
        "full-repo-test-coverage-audit",
        "ui-implementation-audit",
    ] {
        let vendor = root.join(format!("skills/{skill}/scripts/_vendor/full_repo_harness"));
        if vendor.exists() || vendor.symlink_metadata().is_ok() {
            return Err(format!(
                "shared harness copy must not exist: {}",
                vendor.display()
            ));
        }
    }
    Ok(())
}

pub fn check_interaction_parity(root: &Path) -> Result<(), String> {
    let expected = [
        "badge-detail",
        "row-hit-target",
        "navigation-cursor",
        "transient-disclosure",
        "disclosure-scrollbar",
        "icon-meaning",
        "stable-expansion-width",
        "hover-copy",
        "status-summary",
        "message-metadata",
    ];
    if crate::audit_common::INTERACTION_CHECKLIST_LABELS != expected {
        return Err("canonical Rust interaction checklist labels drifted".to_owned());
    }
    let paths = [
        "skills/full-repo-audit/SKILL.md",
        "skills/ui-implementation-audit/SKILL.md",
    ];
    for relative in paths {
        let text = read_text(root, relative)?;
        let missing = crate::audit_common::INTERACTION_CHECKLIST_LABELS
            .iter()
            .filter(|label| !text.contains(**label))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "interaction checklist drift in {relative}: missing={missing:?}"
            ));
        }
    }
    for relative in [
        "rust/tooling/src/audit_queue.rs",
        "rust/tooling/src/audit_verify.rs",
        "rust/tooling/src/ui_audit.rs",
        "rust/tooling/src/ui_audit_verify.rs",
    ] {
        if read_text(root, relative)?.contains("const INTERACTION_CHECKLIST_LABELS") {
            return Err(format!(
                "{relative} redefines the canonical interaction checklist"
            ));
        }
    }
    Ok(())
}

pub fn check_visual_review_parity(root: &Path) -> Result<(), String> {
    let required: [(&str, &[&str]); 6] = [
        (
            "skills/formal-web-ui-verification/SKILL.md",
            &[
                "review-queue.json",
                "manual-review",
                "secondary-workflow-precedes-primary",
            ],
        ),
        (
            "skills/user-journey-docs-audit/SKILL.md",
            &[
                "Formal Web UI verification handoff",
                "continuation anchor",
                "changed visual review",
            ],
        ),
        (
            "skills/ui-implementation-audit/SKILL.md",
            &["review-queue", "runtime/user-selected", "manual-review"],
        ),
        (
            "skills/full-repo-audit/SKILL.md",
            &["Changed Visual Review", "review-queue", "manual-review"],
        ),
        (
            "rust/tooling/src/audit_evidence.rs",
            &[
                "review-queue",
                "manual-review",
                "formal-web-ui-manual-review",
            ],
        ),
        (
            "rust/tooling/src/audit_queue.rs",
            &[
                "Changed Visual Review",
                "review-queue.json",
                "manual-review",
            ],
        ),
    ];
    for (relative, tokens) in required {
        let text = read_text(root, relative)?;
        let missing = tokens
            .iter()
            .filter(|token| !text.contains(**token))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "changed visual-review contract drift in {relative}: missing={missing:?}"
            ));
        }
    }
    Ok(())
}

fn run_command(program: &str, arguments: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .status()
        .map_err(|error| format!("cannot launch {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited {}", status.code().unwrap_or(2)))
    }
}

fn unique_scratch() -> Result<PathBuf, String> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = std::env::temp_dir().join(format!("dc2-skill-validation-{token}"));
    create_directory_all_nofollow(&path, 0o700).map_err(|error| error.to_string())?;
    Ok(path)
}

pub fn check_include_glob_exclusions() -> Result<(), String> {
    let scratch = unique_scratch()?;
    let result = (|| {
        let repo = scratch.join("repo");
        create_directory_all_nofollow(&repo.join("src"), 0o700)
            .map_err(|error| error.to_string())?;
        create_directory_all_nofollow(&repo.join("node_modules/pkg"), 0o700)
            .map_err(|error| error.to_string())?;
        crate::audit_ledger::write_bytes_nofollow(
            &repo.join("src/app.rs"),
            b"fn main() {}\n",
            0o600,
        )
        .map_err(|error| error.to_string())?;
        crate::audit_ledger::write_bytes_nofollow(
            &repo.join("node_modules/pkg/lib.rs"),
            b"pub fn dependency() {}\n",
            0o600,
        )
        .map_err(|error| error.to_string())?;
        run_command("git", &["init", "-q"], &repo)?;
        run_command("git", &["add", "src/app.rs"], &repo)?;
        run_command(
            "git",
            &[
                "-c",
                "user.name=agent-skills-validate",
                "-c",
                "user.email=validate@example.invalid",
                "commit",
                "-q",
                "-m",
                "init",
            ],
            &repo,
        )?;
        let broad = crate::audit_queue::collect_files(
            &repo,
            &crate::audit_queue::CollectOptions {
                include_globs: vec!["**/*.rs".to_owned()],
                ..Default::default()
            },
        );
        if broad
            .entries
            .iter()
            .any(|entry| entry.rel_path == "node_modules/pkg/lib.rs")
        {
            return Err("broad include glob unexpectedly included node_modules".to_owned());
        }
        let explicit = crate::audit_queue::collect_files(
            &repo,
            &crate::audit_queue::CollectOptions {
                include_globs: vec!["node_modules/**/*.rs".to_owned()],
                ..Default::default()
            },
        );
        if !explicit
            .entries
            .iter()
            .any(|entry| entry.rel_path == "node_modules/pkg/lib.rs")
        {
            return Err("explicit include glob did not include its vendor target".to_owned());
        }
        Ok(())
    })();
    let _ = validate_directory_nofollow(&scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

pub fn run_internal_check(root: &Path, name: &str) -> Result<Value, String> {
    match name {
        "repository-layout" => check_repository_layout(root)?,
        "canonical-harness" => check_canonical_harness(root)?,
        "interaction-parity" => check_interaction_parity(root)?,
        "visual-review-parity" => check_visual_review_parity(root)?,
        "include-glob-exclusions" => check_include_glob_exclusions()?,
        _ => return Err(format!("unknown internal validation check: {name}")),
    }
    Ok(json!({"schema":2,"check":name,"status":"passed"}))
}

#[derive(Clone, Debug)]
pub struct ValidationOptions {
    pub root: PathBuf,
    pub current_dir: PathBuf,
    pub tooling_binary: PathBuf,
    pub control_binary: PathBuf,
    pub executor_binary: PathBuf,
    pub coordinator_fixture: PathBuf,
    pub cargo_program: String,
    pub cargo_target_dir: PathBuf,
    pub temp_root: PathBuf,
}

#[derive(Default)]
struct CheckEdges {
    after: Vec<String>,
    requires: Vec<String>,
    invalidates: Vec<String>,
}

fn direct_check(
    name: &str,
    tier: ValidationTier,
    role: CheckRole,
    command: Vec<String>,
    environment: &BTreeMap<String, String>,
    edges: CheckEdges,
    timeout_seconds: u64,
) -> CheckPlan {
    CheckPlan {
        name: name.to_owned(),
        tier,
        role,
        phase: devcoordinator2_executor_protocol::CheckPhase::Check,
        resources: Vec::new(),
        consumes: Vec::new(),
        cacheable: false,
        cache_inputs: Vec::new(),
        fingerprint: String::new(),
        expect_failure: false,
        qualification_of: None,
        after: edges.after,
        requires: edges.requires,
        invalidates: edges.invalidates,
        cwd: ".".to_owned(),
        env: environment.clone(),
        timeout_seconds: Some(timeout_seconds),
        completion: CompletionMode::Process,
        on_failure: FailureMode::Continue,
        produces: Vec::new(),
        retained_artifacts: Vec::new(),
        diagnostic_sources: Vec::new(),
        command: Some(command),
        discover: None,
        case_command: None,
        cases: None,
    }
}

fn tooling_command(options: &ValidationOptions, arguments: &[&str]) -> Vec<String> {
    std::iter::once(options.tooling_binary.to_string_lossy().into_owned())
        .chain(arguments.iter().map(|value| (*value).to_owned()))
        .collect()
}

fn internal_command(options: &ValidationOptions, name: &str) -> Vec<String> {
    tooling_command(
        options,
        &["skills", "validate", "internal-check", name, "--root"],
    )
    .into_iter()
    .chain(std::iter::once(options.root.to_string_lossy().into_owned()))
    .collect()
}

fn cargo_test_command(options: &ValidationOptions, filter: &str) -> Vec<String> {
    let filters: &[&str] = match filter {
        "audit_" => &[
            "audit_common::tests::",
            "audit_evidence::tests::",
            "audit_findings::tests::",
            "audit_ledger::tests::",
            "audit_queue::tests::",
            "audit_targets::tests::",
            "audit_verify::tests::",
        ],
        "ui_" => &[
            "ui_audit::tests::",
            "ui_audit_verify::tests::",
            "ui_gate::tests::",
        ],
        "test_coverage_audit::tests" => &[
            "test_coverage_audit::tests::",
            "test_catalog::tests::",
            "test_assurance::tests::",
        ],
        _ => &[filter],
    };
    let mut command = vec![
        options.cargo_program.clone(),
        "test".to_owned(),
        "--locked".to_owned(),
        "-p".to_owned(),
        "devcoordinator2-tooling".to_owned(),
        "--features".to_owned(),
        "selftest-fixtures".to_owned(),
        "--lib".to_owned(),
        "--".to_owned(),
        "--test-threads=1".to_owned(),
    ];
    command.extend(filters.iter().map(|filter| (*filter).to_owned()));
    // libtest filters are substrings: audit_verify also matches ui_audit_verify.
    if filter == "audit_" {
        command.extend(["--skip".to_owned(), "ui_audit_verify::tests::".to_owned()]);
    }
    command
}

pub fn validation_checks(options: &ValidationOptions) -> Vec<CheckPlan> {
    let temp = options.temp_root.join("skill-validation");
    let mut environment = BTreeMap::from([
        ("TMPDIR".to_owned(), temp.to_string_lossy().into_owned()),
        (
            "CARGO_TARGET_DIR".to_owned(),
            options.cargo_target_dir.to_string_lossy().into_owned(),
        ),
    ]);
    environment.insert("RUST_BACKTRACE".to_owned(), "1".to_owned());
    let root = options.root.to_string_lossy().into_owned();
    let workflow = options.root.join(".github/workflows/validate.yml");
    let ledgers = options.root.join("UserIssueLedgers");
    let mut preflight_specs = vec![
        (
            "repository-layout",
            internal_command(options, "repository-layout"),
        ),
        (
            "policy",
            tooling_command(options, &["check", "app-wide-policy", "--root", &root]),
        ),
        (
            "neutrality",
            tooling_command(options, &["check", "agent-neutrality", "--root", &root]),
        ),
        (
            "ledgers",
            tooling_command(
                options,
                &[
                    "check",
                    "user-issue-ledgers",
                    "--root",
                    &ledgers.to_string_lossy(),
                ],
            ),
        ),
        (
            "boundaries",
            tooling_command(
                options,
                &["check", "repository-boundaries", "--root", &root],
            ),
        ),
        (
            "ci-security",
            tooling_command(
                options,
                &[
                    "check",
                    "ci-security",
                    "--workflow",
                    &workflow.to_string_lossy(),
                ],
            ),
        ),
        (
            "public-artifacts",
            tooling_command(
                options,
                &["check", "public-artifacts", "--repo", &root, "--json"],
            ),
        ),
        (
            "canonical-harness",
            internal_command(options, "canonical-harness"),
        ),
    ];
    let report = options.current_dir.join("python-free-report.json");
    preflight_specs.push((
        "python-free",
        tooling_command(
            options,
            &[
                "check",
                "python-free",
                "--root",
                &root,
                "--report",
                &report.to_string_lossy(),
            ],
        ),
    ));

    let control = options.control_binary.to_string_lossy().into_owned();
    let coordinator_fixture = options.coordinator_fixture.to_string_lossy().into_owned();
    let formal_workspace = temp.join("formal-ui").to_string_lossy().into_owned();
    let mut targets: Vec<(String, Vec<String>, bool)> = vec![
        (
            "interaction-parity".to_owned(),
            internal_command(options, "interaction-parity"),
            false,
        ),
        (
            "visual-review-parity".to_owned(),
            internal_command(options, "visual-review-parity"),
            false,
        ),
        (
            "include-glob-exclusions".to_owned(),
            internal_command(options, "include-glob-exclusions"),
            false,
        ),
        (
            "validator-self-test".to_owned(),
            cargo_test_command(options, "skill_validation::tests"),
            true,
        ),
        (
            "repository-check-self-tests".to_owned(),
            cargo_test_command(options, "repository_checks::tests"),
            true,
        ),
        (
            "skill-link-manager".to_owned(),
            cargo_test_command(options, "skill_links::tests"),
            true,
        ),
        (
            "public-artifact-self-test".to_owned(),
            cargo_test_command(options, "public_artifacts::tests"),
            true,
        ),
        (
            "marker-free-evaluator".to_owned(),
            cargo_test_command(options, "marker_free::tests"),
            true,
        ),
        (
            "skill-dev-coordinator".to_owned(),
            tooling_command(
                options,
                &[
                    "skills",
                    "self-test",
                    "dev-coordinator",
                    "--source-root",
                    &root,
                    "--control-binary",
                    &control,
                ],
            ),
            false,
        ),
        (
            "skill-full-repo-audit".to_owned(),
            cargo_test_command(options, "audit_"),
            true,
        ),
        (
            "skill-full-repo-test-coverage-audit".to_owned(),
            cargo_test_command(options, "test_coverage_audit::tests"),
            true,
        ),
        (
            "skill-ui-implementation-audit".to_owned(),
            cargo_test_command(options, "ui_"),
            true,
        ),
        (
            "skill-user-journey-docs-audit".to_owned(),
            cargo_test_command(options, "journey_docs::tests"),
            true,
        ),
        (
            "skill-formal-web-ui-verification".to_owned(),
            tooling_command(
                options,
                &[
                    "formal-ui",
                    "self-test",
                    "--workspace-parent",
                    &formal_workspace,
                    "--timeout-seconds",
                    "240",
                    "--phase",
                    "all",
                    "--coordinator-fixture",
                    &coordinator_fixture,
                ],
            ),
            false,
        ),
    ];
    let target_names = targets
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect::<Vec<_>>();
    let preflight_names = preflight_specs
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    let mut checks = preflight_specs
        .drain(..)
        .map(|(name, command)| {
            direct_check(
                name,
                ValidationTier::Development,
                CheckRole::Preflight,
                command,
                &environment,
                CheckEdges {
                    invalidates: target_names.clone(),
                    ..Default::default()
                },
                900,
            )
        })
        .collect::<Vec<_>>();
    let formal_after = target_names
        .iter()
        .filter(|name| {
            name.starts_with("skill-") && name.as_str() != "skill-formal-web-ui-verification"
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut previous_cargo = None;
    for (name, command, is_cargo) in targets.drain(..) {
        let after = if name == "skill-formal-web-ui-verification" {
            formal_after.clone()
        } else if is_cargo {
            previous_cargo.iter().cloned().collect()
        } else {
            Vec::new()
        };
        if is_cargo {
            previous_cargo = Some(name.clone());
        }
        let timeout = if name == "skill-formal-web-ui-verification" {
            3600
        } else {
            900
        };
        checks.push(direct_check(
            &name,
            ValidationTier::Release,
            CheckRole::Work,
            command,
            &environment,
            CheckEdges {
                requires: preflight_names.clone(),
                after,
                ..Default::default()
            },
            timeout,
        ));
    }
    checks
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn build_validation_plan(
    options: &ValidationOptions,
    run_id: &str,
    source_digest: &str,
) -> Result<ExecutionPlan, String> {
    let checks = validation_checks(options);
    let config_digest = digest_hex(
        &serde_json::to_vec(&checks).map_err(|error| format!("cannot encode checks: {error}"))?,
    );
    let plan = ExecutionPlan {
        schema: Schema2,
        run_id: run_id.to_owned(),
        test: "agent-skills".to_owned(),
        worktree_root: options.root.to_string_lossy().into_owned(),
        current_dir: options.current_dir.to_string_lossy().into_owned(),
        log_dir: options
            .current_dir
            .join("logs/runs")
            .join(run_id)
            .to_string_lossy()
            .into_owned(),
        requested_tier: ValidationTier::Release,
        readiness_eligible: true,
        proof: ProofKind::Complete,
        selection: Vec::new(),
        origin_run_id: None,
        source_digest: source_digest.to_owned(),
        config_digest,
        reused: BTreeMap::new(),
        case_selection: BTreeMap::new(),
        postgres_databases: BTreeMap::new(),
        reused_qualifications: Default::default(),
        checks,
    };
    plan.validate().map_err(|error| error.to_string())?;
    Ok(plan)
}

pub fn source_digest(executor: &Path, root: &Path) -> Result<String, String> {
    let output = Command::new(executor)
        .args(["source-digest", "--worktree"])
        .arg(root)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run Rust source digest: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Rust source digest failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .rev()
                .take(1000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    let receipt: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Rust source digest returned an invalid receipt".to_owned())?;
    let digest = receipt
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .ok_or_else(|| "Rust source digest returned an invalid sha256".to_owned())?;
    Ok(digest.to_owned())
}

pub fn bounded_receipt(report: &ExecutionReport, report_path: &Path) -> Value {
    let retained = report.failure_index.iter().take(20).collect::<Vec<_>>();
    json!({
        "schema":2,"status":report.status,"checks":report.checks.len(),
        "counts":report.counts,"failure_index":retained,
        "failure_index_truncated":report.failure_index_truncated || report.failure_index.len()>retained.len(),
        "report":report_path,
    })
}

pub fn generate_run_id() -> Result<String, String> {
    use time::{OffsetDateTime, macros::format_description};
    let timestamp = OffsetDateTime::now_utc()
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .map_err(|error| error.to_string())?;
    let mut random = [0u8; 3];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    Ok(format!(
        "skills-{timestamp}-{}-{}",
        std::process::id(),
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn validate_executable(path: &Path, label: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = path
        .symlink_metadata()
        .map_err(|_| format!("{label} is missing: {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(format!(
            "{label} must be a regular executable binary: {}",
            path.display()
        ));
    }
    Ok(())
}

pub struct ValidationRun {
    pub receipt: Value,
    pub exit_code: u8,
    pub plan_path: PathBuf,
}

pub fn write_plan(plan: &ExecutionPlan, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_directory_all_nofollow(parent, 0o700).map_err(|error| error.to_string())?;
    }
    let mut bytes = serde_json::to_vec(plan).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    write_new_bytes_nofollow(path, &bytes, 0o600).map_err(|error| error.to_string())
}

pub fn run_complete(options: &ValidationOptions, run_id: &str) -> Result<ValidationRun, String> {
    validate_executable(&options.executor_binary, "Rust executor")?;
    validate_executable(&options.tooling_binary, "Rust tooling")?;
    validate_executable(&options.control_binary, "Rust control binary")?;
    validate_executable(&options.coordinator_fixture, "Rust coordinator fixture")?;
    let digest = source_digest(&options.executor_binary, &options.root)?;
    let plan = build_validation_plan(options, run_id, &digest)?;
    let plan_path = options.current_dir.join("validation-plan.json");
    write_plan(&plan, &plan_path)?;
    create_directory_all_nofollow(&options.current_dir.join("tmp"), 0o700)
        .map_err(|error| error.to_string())?;
    create_directory_all_nofollow(&options.temp_root.join("skill-validation"), 0o700)
        .map_err(|error| error.to_string())?;
    create_directory_all_nofollow(&PathBuf::from(&plan.log_dir), 0o700)
        .map_err(|error| error.to_string())?;
    let completed = Command::new(&options.executor_binary)
        .arg("run-local")
        .arg(&plan_path)
        .current_dir(&options.root)
        .output()
        .map_err(|error| format!("cannot launch Rust executor: {error}"))?;
    let report_path = options.current_dir.join("check-report.json");
    let bytes = read_bytes_nofollow(&report_path, Some(&options.root))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "Rust executor did not publish check-report.json: {}",
                String::from_utf8_lossy(&completed.stderr)
                    .chars()
                    .rev()
                    .take(1000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            )
        })?;
    let report = ExecutionReport::from_json(&bytes).map_err(|error| error.to_string())?;
    let expected = if report.status == RunStatus::Passed {
        0
    } else {
        1
    };
    let actual = completed
        .status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(2);
    if actual != expected {
        return Err(format!(
            "Rust executor exit/report mismatch ({actual} != {expected})"
        ));
    }
    Ok(ValidationRun {
        receipt: bounded_receipt(&report, &report_path),
        exit_code: expected,
        plan_path,
    })
}

pub fn self_test(executor: &Path, leaf: &Path) -> Result<Value, String> {
    validate_executable(executor, "Rust executor")?;
    validate_executable(leaf, "Rust self-test leaf")?;
    let scratch = unique_scratch()?;
    let result = (|| {
        let repo = scratch.join("repo");
        create_directory_all_nofollow(&repo, 0o700).map_err(|error| error.to_string())?;
        crate::audit_ledger::write_bytes_nofollow(&repo.join("source.txt"), b"fixture\n", 0o600)
            .map_err(|error| error.to_string())?;
        run_command("git", &["init", "-q"], &repo)?;
        run_command("git", &["add", "source.txt"], &repo)?;
        run_command(
            "git",
            &[
                "-c",
                "user.name=validator-self-test",
                "-c",
                "user.email=validator@example.invalid",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
            &repo,
        )?;
        let current = repo.join(".devcoordinator/self-test");
        create_directory_all_nofollow(&current, 0o700).map_err(|error| error.to_string())?;
        let independent = current.join("independent-ran");
        let forbidden = current.join("invalidated-ran");
        let environment = BTreeMap::new();
        let gate = direct_check(
            "gate",
            ValidationTier::Development,
            CheckRole::Preflight,
            vec![leaf.to_string_lossy().into_owned(), "fail".to_owned()],
            &environment,
            CheckEdges {
                invalidates: vec!["expensive".to_owned()],
                ..Default::default()
            },
            30,
        );
        let expensive = direct_check(
            "expensive",
            ValidationTier::Development,
            CheckRole::Work,
            vec![
                leaf.to_string_lossy().into_owned(),
                "write".to_owned(),
                forbidden.to_string_lossy().into_owned(),
                "bad".to_owned(),
            ],
            &environment,
            CheckEdges {
                requires: vec!["gate".to_owned()],
                ..Default::default()
            },
            30,
        );
        let independent_check = direct_check(
            "independent",
            ValidationTier::Development,
            CheckRole::Work,
            vec![
                leaf.to_string_lossy().into_owned(),
                "write".to_owned(),
                independent.to_string_lossy().into_owned(),
                "ok".to_owned(),
            ],
            &environment,
            CheckEdges::default(),
            30,
        );
        let run_id = "validator-self-test";
        let plan = ExecutionPlan {
            schema: Schema2,
            run_id: run_id.to_owned(),
            test: "validator".to_owned(),
            worktree_root: repo.to_string_lossy().into_owned(),
            current_dir: current.to_string_lossy().into_owned(),
            log_dir: current
                .join("logs/runs/validator-self-test")
                .to_string_lossy()
                .into_owned(),
            requested_tier: ValidationTier::Release,
            readiness_eligible: true,
            proof: ProofKind::Complete,
            selection: Vec::new(),
            origin_run_id: None,
            source_digest: source_digest(executor, &repo)?,
            config_digest: "b".repeat(64),
            reused: BTreeMap::new(),
            case_selection: BTreeMap::new(),
            postgres_databases: BTreeMap::new(),
            reused_qualifications: Default::default(),
            checks: vec![gate, expensive, independent_check],
        };
        plan.validate().map_err(|error| error.to_string())?;
        let plan_path = current.join("plan.json");
        write_plan(&plan, &plan_path)?;
        create_directory_all_nofollow(&PathBuf::from(&plan.log_dir), 0o700)
            .map_err(|error| error.to_string())?;
        let completed = Command::new(executor)
            .arg("run-local")
            .arg(&plan_path)
            .current_dir(&repo)
            .output()
            .map_err(|error| error.to_string())?;
        if completed.status.code() != Some(1) {
            return Err("invalidation fixture executor exit was not 1".to_owned());
        }
        let report_path = current.join("check-report.json");
        let report = ExecutionReport::from_json(
            &read_bytes_nofollow(&report_path, Some(&repo))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "invalidation fixture report is missing".to_owned())?,
        )
        .map_err(|error| error.to_string())?;
        let states = report
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.status))
            .collect::<BTreeMap<_, _>>();
        if states.get("gate") != Some(&LeafStatus::Failed)
            || states.get("expensive") != Some(&LeafStatus::Invalidated)
            || states.get("independent") != Some(&LeafStatus::Passed)
        {
            return Err(format!(
                "invalidation/all-settled states are wrong: {states:?}"
            ));
        }
        if read_bytes_nofollow(&independent, Some(&repo))
            .map_err(|error| error.to_string())?
            .as_deref()
            != Some(b"ok")
            || forbidden.exists()
            || report.source_changed
            || !PathBuf::from(&plan.log_dir)
                .join("checks/gate/check/stderr.log")
                .is_file()
        {
            return Err(
                "invalidation fixture lost independent work, isolation, or logs".to_owned(),
            );
        }
        Ok(
            json!({"ok":true,"states":{"gate":"failed","expensive":"invalidated","independent":"passed"}}),
        )
    })();
    match result {
        Ok(value) => {
            let _ = std::fs::remove_dir_all(&scratch);
            Ok(value)
        }
        Err(error) => Err(format!(
            "{error}; preserved validator self-test: {}",
            scratch.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_skill_filters_partition_actual_collected_tests() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--list", "--format", "terse"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let list = String::from_utf8(output.stdout).unwrap();
        let options = options();
        let groups = ["audit_", "ui_", "test_coverage_audit::tests"].map(|filter| {
            let mut args = cargo_test_command(&options, filter)
                .into_iter()
                .skip_while(|arg| arg != "--")
                .skip(1);
            let (mut filters, mut skips) = (Vec::new(), Vec::new());
            while let Some(arg) = args.next() {
                if arg == "--skip" {
                    skips.push(args.next().unwrap());
                } else if !arg.starts_with('-') {
                    filters.push(arg);
                }
            }
            (filters, skips)
        });
        let mut assurance_count = 0;
        for name in list.lines().filter_map(|line| line.strip_suffix(": test")) {
            let owners = groups
                .iter()
                .filter(|(filters, skips)| {
                    filters.iter().any(|filter| name.contains(filter))
                        && !skips.iter().any(|skip| name.contains(skip))
                })
                .count();
            assert!(
                owners <= 1,
                "test selected by multiple audit skills: {name}"
            );
            if name.starts_with("test_assurance::tests::")
                || name.starts_with("test_catalog::tests::")
            {
                assert_eq!(owners, 1);
                assurance_count += 1;
            }
        }
        assert!(assurance_count > 0);
    }

    fn options() -> ValidationOptions {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_owned();
        ValidationOptions {
            current_dir: root.join(".devcoordinator/agent-validation/shape-only"),
            tooling_binary: PathBuf::from("/opt/devcoordinator2-tooling"),
            control_binary: PathBuf::from("/opt/devcoordinator2"),
            executor_binary: PathBuf::from("/opt/devcoordinator2-executor"),
            coordinator_fixture: PathBuf::from("/opt/devcoordinator2-selftest-coordinator"),
            cargo_program: "cargo".to_owned(),
            cargo_target_dir: root.join("target/agent-validation"),
            temp_root: std::env::temp_dir().join("devcoordinator2-agent-validation-tests"),
            root,
        }
    }

    #[test]
    fn plan_is_strict_python_free_schema_two_with_complete_invalidation_edges() {
        let options = options();
        let plan = build_validation_plan(&options, "shape-only", &"a".repeat(64)).unwrap();
        assert_eq!(plan.proof, ProofKind::Complete);
        assert_eq!(plan.requested_tier, ValidationTier::Release);
        assert!(plan.readiness_eligible);
        assert_eq!(
            plan.log_dir,
            options
                .current_dir
                .join("logs/runs/shape-only")
                .to_string_lossy()
        );
        let preflights = plan
            .checks
            .iter()
            .filter(|check| check.role == CheckRole::Preflight)
            .collect::<Vec<_>>();
        let targets = plan
            .checks
            .iter()
            .filter(|check| check.role == CheckRole::Work)
            .collect::<Vec<_>>();
        assert!(preflights.len() >= 9);
        assert!(targets.len() >= 14);
        let preflight_names = preflights
            .iter()
            .map(|check| check.name.clone())
            .collect::<BTreeSet<_>>();
        let target_names = targets
            .iter()
            .map(|check| check.name.clone())
            .collect::<BTreeSet<_>>();
        assert!(preflights.iter().all(|check| {
            check.invalidates.iter().cloned().collect::<BTreeSet<_>>() == target_names
        }));
        assert!(targets.iter().all(|check| {
            check.requires.iter().cloned().collect::<BTreeSet<_>>() == preflight_names
        }));
        let formal = targets
            .iter()
            .find(|check| check.name == "skill-formal-web-ui-verification")
            .unwrap();
        let expected = targets
            .iter()
            .filter(|check| check.name.starts_with("skill-") && check.name != formal.name)
            .map(|check| check.name.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            formal.after.iter().cloned().collect::<BTreeSet<_>>(),
            expected
        );
        assert!(
            plan.checks
                .iter()
                .all(|check| check.on_failure == FailureMode::Continue)
        );
        let commands = plan
            .checks
            .iter()
            .flat_map(|check| check.command.as_deref().unwrap_or_default())
            .cloned()
            .collect::<Vec<_>>();
        assert!(!commands.iter().any(|argument| {
            argument == "python"
                || argument == "python3"
                || argument.ends_with(".py")
                || argument == "compileall"
        }));
        assert!(plan.checks.iter().any(|check| check.name == "python-free"));
    }

    #[test]
    fn current_rust_internal_checks_and_include_glob_fixture_pass() {
        let options = options();
        for name in [
            "repository-layout",
            "canonical-harness",
            "interaction-parity",
            "visual-review-parity",
        ] {
            assert_eq!(
                run_internal_check(&options.root, name).unwrap()["status"],
                "passed"
            );
        }
        check_include_glob_exclusions().unwrap();
    }
}
