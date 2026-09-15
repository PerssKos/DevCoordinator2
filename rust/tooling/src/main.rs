use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

const SOURCE_COMMIT: &str = match option_env!("DEVCOORDINATOR2_SOURCE_COMMIT") {
    Some(value) => value,
    None => "development",
};

#[derive(Debug, Parser)]
#[command(name = "devcoordinator2-tooling", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    FormalUi {
        #[command(subcommand)]
        command: FormalUiCommand,
    },
    Contract {
        #[command(subcommand)]
        command: ContractCommand,
    },
    Check {
        #[command(subcommand)]
        command: CheckCommand,
    },
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    Legacy {
        #[command(subcommand)]
        command: LegacyCommand,
    },
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    Planning {
        #[command(subcommand)]
        command: PlanningCommand,
    },
    Install {
        #[command(subcommand)]
        command: InstallCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PlanningCommand {
    /// Inspect counts and requested IDs in a saved activation backup. Never restore data.
    InspectBackup {
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        repository_id: String,
        #[arg(long)]
        task_id: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum FormalUiCommand {
    /// Exercise the retained browser verifier with Rust-hosted realistic fixtures.
    SelfTest {
        #[arg(long)]
        workspace_parent: Option<PathBuf>,
        #[arg(long)]
        keep: bool,
        #[arg(long, default_value_t = 240)]
        timeout_seconds: u64,
        #[arg(long, default_value = "all")]
        phase: String,
        #[arg(long)]
        coordinator_fixture: Option<PathBuf>,
    },
    /// Run the retained Node browser verifier through the Rust tooling surface.
    Verify {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Finalize agent decisions or validate an existing manual-review manifest.
    Review {
        #[arg(long)]
        report: PathBuf,
        #[arg(long)]
        queue: PathBuf,
        #[arg(long, conflicts_with = "review")]
        decisions: Option<PathBuf>,
        #[arg(long, conflicts_with = "decisions")]
        review: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum AuditCommand {
    MarkerFree {
        #[command(subcommand)]
        command: MarkerFreeCommand,
    },
    UiImplementation {
        #[command(subcommand)]
        command: UiImplementationCommand,
    },
    TestCoverage {
        #[command(subcommand)]
        command: TestCoverageCommand,
    },
    JourneyDocs {
        #[command(subcommand)]
        command: JourneyDocsCommand,
    },
    /// Build a deterministic full-repository audit queue and manifest.
    BuildFullRepo {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        batch_size: usize,
        #[arg(long, default_value_t = 60_000)]
        max_batch_bytes: usize,
        #[arg(long, overrides_with = "no_include_config")]
        include_config: bool,
        #[arg(long, overrides_with = "include_config")]
        no_include_config: bool,
        #[arg(long)]
        include_env: bool,
        #[arg(long)]
        include_generated: bool,
        #[arg(long)]
        include_vendor: bool,
        #[arg(long)]
        include_assets: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long = "exclude-glob")]
        exclude_globs: Vec<String>,
        #[arg(long = "include-file")]
        include_files: Vec<String>,
        #[arg(long = "include-glob")]
        include_globs: Vec<String>,
    },
    /// Validate full-repository batch, journey, lead, and receipt artifacts.
    VerifyFullRepo {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, required = true, num_args = 1..)]
        reports: Vec<PathBuf>,
        #[arg(long)]
        batch_id: Option<String>,
        #[arg(long)]
        skip_current_hash_check: bool,
        #[arg(long)]
        receipt_out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Merge, rank, hash-bind, and optionally project verified audit findings.
    MergeFindings {
        #[arg(long)]
        reports: PathBuf,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        markdown_out: Option<PathBuf>,
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        verification_receipt: Option<PathBuf>,
        #[arg(long)]
        ledger_projection_out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MarkerFreeCommand {
    ValidateSuite {
        #[arg(long)]
        suite_root: Option<PathBuf>,
    },
    ValidateResponse {
        #[arg(long)]
        suite_root: Option<PathBuf>,
        #[arg(long = "case")]
        case_id: String,
        #[arg(long)]
        response: PathBuf,
    },
    Score {
        #[arg(long)]
        suite_root: Option<PathBuf>,
        #[arg(long)]
        responses: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum UiImplementationCommand {
    Build {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 6)]
        batch_size: usize,
        #[arg(long, default_value_t = 60_000)]
        max_batch_bytes: usize,
        #[arg(long, overrides_with = "no_include_config")]
        include_config: bool,
        #[arg(long, overrides_with = "include_config")]
        no_include_config: bool,
        #[arg(long)]
        include_env: bool,
        #[arg(long)]
        include_generated: bool,
        #[arg(long)]
        include_vendor: bool,
        #[arg(long, overrides_with = "no_include_assets")]
        include_assets: bool,
        #[arg(long, overrides_with = "include_assets")]
        no_include_assets: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long = "exclude-glob")]
        exclude_globs: Vec<String>,
        #[arg(long = "include-file")]
        include_files: Vec<String>,
        #[arg(long = "include-glob")]
        include_globs: Vec<String>,
        #[arg(long)]
        mockup: Vec<String>,
        #[arg(long = "journey-file")]
        journey_files: Vec<String>,
        #[arg(long)]
        split_visual_discovery: bool,
        #[arg(long)]
        ui_platform: Option<String>,
        #[arg(long)]
        formal_config: Option<String>,
        #[arg(long = "implemented-ui-file")]
        implemented_ui_files: Vec<String>,
        #[arg(long = "implemented-ui-override", num_args = 3, action = clap::ArgAction::Append)]
        implemented_ui_overrides: Vec<String>,
        #[arg(long)]
        eligibility_only: bool,
    },
    ImportFormal {
        #[arg(long)]
        audit_root: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        formal_report: PathBuf,
        #[arg(long)]
        journey_evidence: PathBuf,
        #[arg(long)]
        review_queue: PathBuf,
        #[arg(long)]
        manual_review: PathBuf,
    },
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, required = true, num_args = 1..)]
        reports: Vec<PathBuf>,
        #[arg(long)]
        skip_current_hash_check: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TestCoverageCommand {
    Build {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        batch_size: usize,
        #[arg(long, default_value_t = 60_000)]
        max_batch_bytes: usize,
        #[arg(long, overrides_with = "no_include_config")]
        include_config: bool,
        #[arg(long, overrides_with = "include_config")]
        no_include_config: bool,
        #[arg(long)]
        include_env: bool,
        #[arg(long)]
        include_generated: bool,
        #[arg(long)]
        include_vendor: bool,
        #[arg(long)]
        include_assets: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long = "exclude-glob")]
        exclude_globs: Vec<String>,
        #[arg(long = "include-file")]
        include_files: Vec<String>,
        #[arg(long = "include-glob")]
        include_globs: Vec<String>,
        #[arg(long = "coverage-report")]
        coverage_reports: Vec<PathBuf>,
        /// Reviewed scope, native test receipts, UI requirements and project budgets.
        #[arg(long)]
        assurance_input: Option<PathBuf>,
    },
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long, required = true, num_args = 1..)]
        reports: Vec<PathBuf>,
        #[arg(long)]
        skip_current_hash_check: bool,
        /// Exit 3 when a valid audit has coverage, UI or efficiency gaps/unknowns.
        #[arg(long)]
        require_assurance: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum JourneyDocsCommand {
    /// Inventory existing product and journey documentation.
    Inventory {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Verify a finished journey-documentation audit report.
    Verify { report: PathBuf },
}

#[derive(Debug, Subcommand)]
enum InstallCommand {
    Configure {
        #[arg(long, default_value = ".")]
        source_root: PathBuf,
        #[arg(long)]
        base_domain: String,
        #[arg(long)]
        admin_emails: String,
        #[arg(long)]
        client_accounts: String,
        #[arg(long, default_value = "devcoordinator2-clients")]
        client_group: String,
        #[arg(long)]
        canary: bool,
        #[arg(long, default_value_t = 28080)]
        canary_port: u16,
        #[arg(long = "compose-env-authorization")]
        compose_env_authorizations: Vec<String>,
        #[arg(long = "codex-usage-account")]
        codex_usage_accounts: Vec<String>,
    },
    Preflight {
        #[arg(long, default_value = ".")]
        source_root: PathBuf,
        #[arg(long)]
        fetch: bool,
    },
    Build {
        #[arg(long, default_value = ".")]
        source_root: PathBuf,
        #[arg(
            long,
            default_value = "/etc/devcoordinator2/candidate-install-manifest.json"
        )]
        manifest: PathBuf,
    },
    Verify {
        #[arg(
            long,
            default_value = "/etc/devcoordinator2/candidate-install-manifest.json"
        )]
        manifest: PathBuf,
    },
    Plan {
        #[arg(
            long,
            default_value = "/etc/devcoordinator2/candidate-install-manifest.json"
        )]
        manifest: PathBuf,
    },
    Activate {
        #[arg(
            long,
            default_value = "/etc/devcoordinator2/candidate-install-manifest.json"
        )]
        candidate_manifest: PathBuf,
        #[arg(long)]
        transaction_dir: Option<PathBuf>,
        #[arg(long)]
        canary: bool,
        #[arg(long)]
        yes: bool,
    },
    Recover {
        #[arg(long)]
        transaction_dir: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum LegacyCommand {
    Export(LegacyExportArgs),
    Import(LegacyImportArgs),
}

#[derive(Debug, Args)]
struct LegacyExportArgs {
    #[arg(long)]
    authority_db: PathBuf,
    #[arg(long)]
    routes_publication: Option<PathBuf>,
    #[arg(long)]
    access_control: Option<PathBuf>,
    #[arg(long)]
    telegram_state: Option<PathBuf>,
    #[arg(long)]
    bugs_dir: Option<PathBuf>,
    #[arg(long = "out")]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct LegacyImportArgs {
    #[arg(long = "export")]
    export_file: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    bugs_dir: PathBuf,
    #[arg(long)]
    live_containers: Option<PathBuf>,
    #[arg(long)]
    current_route_map: Option<PathBuf>,
    #[arg(long)]
    routes_path: Option<PathBuf>,
    #[arg(long)]
    base_domain: Option<String>,
    #[arg(long)]
    prune_missing_install_fixtures: bool,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum DecisionCommand {
    Import(DecisionImportArgs),
}

#[derive(Debug, Args)]
struct DecisionImportArgs {
    #[arg(long)]
    path: PathBuf,
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "aspect")]
    aspect_overrides: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    Links {
        #[command(subcommand)]
        command: SkillLinksCommand,
    },
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    SelfTest {
        #[command(subcommand)]
        command: SkillSelfTestCommand,
    },
    Validate {
        #[command(subcommand)]
        command: SkillValidationCommand,
    },
}

#[derive(Debug, Args)]
struct SkillValidationArgs {
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    executor: Option<PathBuf>,
    #[arg(long)]
    tooling: Option<PathBuf>,
    #[arg(long)]
    control: Option<PathBuf>,
    #[arg(long)]
    coordinator_fixture: Option<PathBuf>,
    #[arg(long, default_value = "cargo")]
    cargo: String,
    #[arg(long)]
    cargo_target_dir: Option<PathBuf>,
    #[arg(long)]
    run_root: Option<PathBuf>,
    #[arg(long)]
    temp_root: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum SkillValidationCommand {
    Run {
        #[command(flatten)]
        options: SkillValidationArgs,
    },
    Plan {
        #[command(flatten)]
        options: SkillValidationArgs,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        run_id: Option<String>,
    },
    InternalCheck {
        name: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    SelfTest {
        #[arg(long)]
        executor: Option<PathBuf>,
        #[arg(long)]
        leaf_fixture: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum SkillSelfTestCommand {
    DevCoordinator {
        #[arg(long, default_value = ".")]
        source_root: PathBuf,
        #[arg(long)]
        control_binary: Option<PathBuf>,
    },
    /// Run the complete Rust self-test matrix shared by the four audit skills.
    AuditTooling {
        #[arg(long, default_value = ".")]
        source_root: PathBuf,
        #[arg(long, default_value = "cargo")]
        cargo: String,
        #[arg(long)]
        cargo_target_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
struct SkillSelection {
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,
    #[arg(long, required = true)]
    target_root: Vec<PathBuf>,
    #[arg(long)]
    skill: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum SkillLinksCommand {
    Plan(SkillSelection),
    Verify(SkillSelection),
    Apply {
        #[command(flatten)]
        selection: SkillSelection,
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        allow_noncanonical: bool,
    },
    Rollback {
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PolicyCommand {
    Plan {
        #[arg(long)]
        repo_root: PathBuf,
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        codex_target: Vec<PathBuf>,
        #[arg(long)]
        claude_target: Vec<PathBuf>,
        #[arg(long)]
        claude_windows_import_wrapper_target: Vec<PathBuf>,
    },
    Apply {
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        plan_digest: String,
    },
    Verify {
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        plan_digest: String,
    },
    Rollback {
        #[arg(long)]
        transaction_dir: PathBuf,
        #[arg(long)]
        plan_digest: String,
    },
}

#[derive(Debug, Subcommand)]
enum ContractCommand {
    Export {
        #[arg(long, default_value = "contracts/devcoordinator2-v2.schema.json")]
        output: PathBuf,
        #[arg(long)]
        check: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CheckCommand {
    PublicArtifacts {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        allow_internal_symlinks: bool,
        #[arg(long)]
        json: bool,
    },
    /// Reject every executable Python dependency while retaining the seven
    /// approved inert audit fixtures.
    PythonFree {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    NoInstanceData {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        patterns: Option<PathBuf>,
    },
    NoTestTimerWaits {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    AgentNeutrality {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    AppWidePolicy {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long)]
        project_importer: Option<PathBuf>,
    },
    CiSecurity {
        #[arg(long, default_value = ".github/workflows/validate.yml")]
        workflow: PathBuf,
    },
    RepositoryBoundaries {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    RepositoryFreshness {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long)]
        branch: Option<String>,
    },
    UserIssueLedgers {
        #[arg(long, default_value = "UserIssueLedgers")]
        root: PathBuf,
    },
}

fn main() -> ExitCode {
    if source_commit_requested() {
        println!("{SOURCE_COMMIT}");
        return ExitCode::SUCCESS;
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Audit { command } => run_audit(command),
        Command::FormalUi { command } => run_formal_ui(command),
        Command::Contract {
            command: ContractCommand::Export { output, check },
        } => match devcoordinator2_tooling::export_contract(&output, check) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        },
        Command::Check {
            command:
                CheckCommand::PublicArtifacts {
                    repo,
                    allow_internal_symlinks,
                    json: json_output,
                },
        } => {
            match devcoordinator2_tooling::public_artifacts::scan(&repo, allow_internal_symlinks) {
                Ok(report) => {
                    if json_output {
                        println!("{}", serde_json::to_string_pretty(&report).unwrap());
                    } else if report["ok"] == true {
                        println!(
                            "public artifact guard ok ({} publishable files)",
                            report["scanned"]
                        );
                    } else if let Some(findings) = report["findings"].as_array() {
                        for finding in findings {
                            let location = finding["line"].as_u64().map_or_else(
                                || finding["path"].as_str().unwrap_or("").to_owned(),
                                |line| format!("{}:{line}", finding["path"].as_str().unwrap_or("")),
                            );
                            println!(
                                "{location}: {}: {}",
                                finding["rule"].as_str().unwrap_or("invalid"),
                                finding["detail"].as_str().unwrap_or("")
                            );
                        }
                    }
                    if report["ok"] == true {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(error) => tooling_error(&format!("public artifact guard failed: {error}"), 2),
            }
        }
        Command::Check {
            command: CheckCommand::PythonFree { root, report },
        } => {
            let report = report.unwrap_or_else(|| {
                root.join(devcoordinator2_tooling::python_guard::DEFAULT_REPORT_RELATIVE_PATH)
            });
            match devcoordinator2_tooling::python_guard::inspect_repository_to_report(
                &root, &report,
            ) {
                Ok(receipt) => {
                    println!("{}", receipt.to_json());
                    if receipt.is_clean() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": error.kind.as_str(),
                                "message": error.to_string(),
                            }
                        })
                    );
                    ExitCode::from(2)
                }
            }
        }
        Command::Check {
            command: CheckCommand::NoInstanceData { root, patterns },
        } => {
            let patterns = patterns.unwrap_or_else(|| root.join("instance/forbidden-strings.txt"));
            check_report(
                devcoordinator2_tooling::repository_checks::check_no_instance_data(
                    &root, &patterns,
                ),
            )
        }
        Command::Check {
            command: CheckCommand::NoTestTimerWaits { root },
        } => check_report(
            devcoordinator2_tooling::repository_checks::check_no_test_timer_waits(&root),
        ),
        Command::Check {
            command: CheckCommand::AgentNeutrality { root },
        } => {
            check_report(devcoordinator2_tooling::repository_checks::audit_agent_neutrality(&root))
        }
        Command::Check {
            command:
                CheckCommand::AppWidePolicy {
                    root,
                    policy,
                    project_importer,
                },
        } => {
            let policy = policy.unwrap_or_else(|| root.join("reference/universal/AGENTS.md"));
            let importer = project_importer.or_else(|| {
                (policy == root.join("reference/universal/AGENTS.md"))
                    .then(|| root.join("CLAUDE.md"))
            });
            let report = devcoordinator2_tooling::repository_checks::check_app_wide_policy(
                &policy,
                importer.as_deref(),
            );
            emit_report(report.to_json(), report.is_clean(), 1)
        }
        Command::Check {
            command: CheckCommand::CiSecurity { workflow },
        } => match devcoordinator2_tooling::repository_checks::check_ci_workflow(&workflow) {
            Ok(result) => emit_report(result.to_json(), result.report.is_clean(), 1),
            Err(error) => check_error(error),
        },
        Command::Check {
            command: CheckCommand::RepositoryBoundaries { root },
        } => check_report(
            devcoordinator2_tooling::repository_checks::audit_repository_boundaries(&root),
        ),
        Command::Check {
            command:
                CheckCommand::RepositoryFreshness {
                    root,
                    remote,
                    branch,
                },
        } => match devcoordinator2_tooling::repository_checks::inspect_repository_freshness(
            &root,
            &remote,
            branch.as_deref(),
        ) {
            Ok(result) => emit_report(result.to_json(), result.exit_code == 0, result.exit_code),
            Err(error) => check_error(error),
        },
        Command::Check {
            command: CheckCommand::UserIssueLedgers { root },
        } => {
            let result =
                devcoordinator2_tooling::repository_checks::audit_user_issue_ledgers(&root);
            emit_report(result.to_json(), result.is_clean(), 1)
        }
        Command::Skills {
            command: SkillsCommand::Links { command },
        } => run_skill_links(command),
        Command::Skills {
            command: SkillsCommand::Policy { command },
        } => run_policy(command),
        Command::Skills {
            command: SkillsCommand::SelfTest { command },
        } => run_skill_self_test(command),
        Command::Skills {
            command: SkillsCommand::Validate { command },
        } => run_skill_validation(command),
        Command::Legacy { command } => run_legacy(command),
        Command::Decision { command } => run_decision(command),
        Command::Planning { command } => run_planning(command),
        Command::Install { command } => run_install(command),
    }
}

fn run_skill_self_test(command: SkillSelfTestCommand) -> ExitCode {
    match command {
        SkillSelfTestCommand::DevCoordinator {
            source_root,
            control_binary,
        } => {
            let result: Result<serde_json::Value, String> = (|| {
                let source_root = source_root
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve source root: {error}"))?;
                let control_binary = match control_binary {
                    Some(path) => path,
                    None => devcoordinator2_tooling::skill_selftest::sibling_control_binary()?,
                };
                devcoordinator2_tooling::skill_selftest::dev_coordinator(
                    &source_root,
                    &control_binary,
                )
            })();
            match result {
                Ok(result) => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap());
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 1),
            }
        }
        SkillSelfTestCommand::AuditTooling {
            source_root,
            cargo,
            cargo_target_dir,
        } => {
            let result = (|| {
                let source_root = source_root
                    .canonicalize()
                    .map_err(|error| format!("cannot resolve source root: {error}"))?;
                devcoordinator2_tooling::skill_selftest::audit_tooling(
                    &source_root,
                    &cargo,
                    cargo_target_dir.as_deref(),
                )
            })();
            match result {
                Ok(result) => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap());
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 1),
            }
        }
    }
}

fn validation_options(
    options: SkillValidationArgs,
    run_id: &str,
) -> Result<devcoordinator2_tooling::skill_validation::ValidationOptions, String> {
    let root = options
        .root
        .canonicalize()
        .map_err(|error| format!("cannot resolve validation root: {error}"))?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let binary_dir = executable
        .parent()
        .ok_or_else(|| "tooling executable has no parent directory".to_owned())?;
    let binary_dir = binary_dir.to_owned();
    let resolve_under_root = |value: Option<PathBuf>, default: PathBuf| {
        let path = value.unwrap_or(default);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    let run_root = resolve_under_root(
        options.run_root,
        root.join(".devcoordinator/agent-validation"),
    );
    let cargo_target_dir = resolve_under_root(
        options.cargo_target_dir,
        root.join("target/agent-validation-cargo"),
    );
    let temp_root = options
        .temp_root
        .unwrap_or_else(|| std::env::temp_dir().join("devcoordinator2-agent-validation"));
    Ok(
        devcoordinator2_tooling::skill_validation::ValidationOptions {
            root,
            current_dir: run_root.join(run_id),
            tooling_binary: options.tooling.unwrap_or(executable),
            control_binary: options
                .control
                .unwrap_or_else(|| binary_dir.join("devcoordinator2")),
            executor_binary: options
                .executor
                .unwrap_or_else(|| binary_dir.join("devcoordinator2-executor")),
            coordinator_fixture: options
                .coordinator_fixture
                .unwrap_or_else(|| binary_dir.join("devcoordinator2-selftest-coordinator")),
            cargo_program: options.cargo,
            cargo_target_dir,
            temp_root,
        },
    )
}

fn run_skill_validation(command: SkillValidationCommand) -> ExitCode {
    use devcoordinator2_tooling::skill_validation;
    match command {
        SkillValidationCommand::InternalCheck { name, root } => {
            let root = root.canonicalize().map_err(|error| error.to_string());
            match root.and_then(|root| skill_validation::run_internal_check(&root, &name)) {
                Ok(result) => {
                    println!("{}", result);
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 1),
            }
        }
        SkillValidationCommand::SelfTest {
            executor,
            leaf_fixture,
        } => {
            let result = (|| {
                let executable = std::env::current_exe().map_err(|error| error.to_string())?;
                let directory = executable
                    .parent()
                    .ok_or_else(|| "tooling executable has no parent directory".to_owned())?;
                skill_validation::self_test(
                    &executor.unwrap_or_else(|| directory.join("devcoordinator2-executor")),
                    &leaf_fixture
                        .unwrap_or_else(|| directory.join("devcoordinator2-selftest-leaf")),
                )
            })();
            match result {
                Ok(result) => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap());
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 1),
            }
        }
        SkillValidationCommand::Plan {
            options,
            output,
            run_id,
        } => {
            let result: Result<serde_json::Value, String> = (|| {
                let run_id = run_id.unwrap_or(skill_validation::generate_run_id()?);
                let options = validation_options(options, &run_id)?;
                let digest =
                    skill_validation::source_digest(&options.executor_binary, &options.root)?;
                let plan = skill_validation::build_validation_plan(&options, &run_id, &digest)?;
                let path =
                    output.unwrap_or_else(|| options.current_dir.join("validation-plan.json"));
                skill_validation::write_plan(&plan, &path)?;
                Ok(json!({"schema":2,"run_id":run_id,"checks":plan.checks.len(),"plan":path}))
            })();
            match result {
                Ok(result) => {
                    println!("{}", result);
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 2),
            }
        }
        SkillValidationCommand::Run { options } => {
            let result = (|| {
                let run_id = skill_validation::generate_run_id()?;
                let options = validation_options(options, &run_id)?;
                skill_validation::run_complete(&options, &run_id)
            })();
            match result {
                Ok(run) => {
                    println!("{}", run.receipt);
                    ExitCode::from(run.exit_code)
                }
                Err(error) => tooling_error(&error, 2),
            }
        }
    }
}

fn run_formal_ui(command: FormalUiCommand) -> ExitCode {
    match command {
        FormalUiCommand::SelfTest {
            workspace_parent,
            keep,
            timeout_seconds,
            phase,
            coordinator_fixture,
        } => match devcoordinator2_tooling::formal_selftest::run(
            &devcoordinator2_tooling::formal_selftest::SelfTestOptions {
                workspace_parent,
                keep,
                timeout_seconds,
                phase,
                coordinator_fixture,
            },
        ) {
            Ok(result) => {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                ExitCode::SUCCESS
            }
            Err(error) => formal_ui_error(&error),
        },
        FormalUiCommand::Verify { arguments } => {
            let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_owned);
            let Some(source_root) = source_root else {
                return formal_ui_error("cannot resolve the canonical tooling source root");
            };
            let verifier = source_root
                .join("skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
            if !verifier.is_file() || verifier.is_symlink() {
                return formal_ui_error(&format!(
                    "retained Node verifier is unavailable: {}",
                    verifier.display()
                ));
            }
            match std::process::Command::new("node")
                .arg(verifier)
                .args(arguments)
                .status()
            {
                Ok(status) => ExitCode::from(
                    status
                        .code()
                        .and_then(|code| u8::try_from(code).ok())
                        .unwrap_or(2),
                ),
                Err(error) => formal_ui_error(&format!("cannot launch Node verifier: {error}")),
            }
        }
        FormalUiCommand::Review {
            report,
            queue,
            decisions,
            review,
            out,
        } => {
            let result = (|| {
                if let Some(review) = review {
                    if out.is_some() {
                        return Err("--out is only valid with --decisions".to_owned());
                    }
                    return devcoordinator2_tooling::formal_review::validate(
                        &review, &report, &queue,
                    );
                }
                let decisions = decisions.ok_or_else(|| {
                    "exactly one of --decisions or --review is required".to_owned()
                })?;
                let out = out.ok_or_else(|| "--out is required with --decisions".to_owned())?;
                let review = devcoordinator2_tooling::formal_review::finalize(
                    &report,
                    &queue,
                    Some(&decisions),
                    &timestamp()?,
                )?;
                devcoordinator2_tooling::formal_review::write_new_review(&out, &review)?;
                Ok(review)
            })();
            match result {
                Ok(review) => match devcoordinator2_tooling::formal_review::summary(&review) {
                    Ok(summary) => {
                        println!("{summary}");
                        if summary.get("ok") == Some(&serde_json::json!(true)) {
                            ExitCode::SUCCESS
                        } else {
                            ExitCode::from(1)
                        }
                    }
                    Err(error) => formal_ui_error(&error),
                },
                Err(error) => formal_ui_error(&error),
            }
        }
    }
}

fn formal_ui_error(error: &str) -> ExitCode {
    println!("{}", serde_json::json!({"ok":false,"error":error}));
    ExitCode::from(2)
}

fn run_audit(command: AuditCommand) -> ExitCode {
    match command {
        AuditCommand::MarkerFree { command } => run_marker_free(command),
        AuditCommand::UiImplementation { command } => run_ui_implementation(command),
        AuditCommand::TestCoverage { command } => run_test_coverage(command),
        AuditCommand::JourneyDocs { command } => run_journey_docs(command),
        AuditCommand::BuildFullRepo {
            repo,
            out,
            batch_size,
            max_batch_bytes,
            include_config: _,
            no_include_config,
            include_env,
            include_generated,
            include_vendor,
            include_assets,
            run_id,
            exclude_globs,
            include_files,
            include_globs,
        } => run_full_repo_build(
            repo,
            out,
            batch_size,
            max_batch_bytes,
            !no_include_config,
            include_env,
            include_generated,
            include_vendor,
            include_assets,
            run_id,
            exclude_globs,
            include_files,
            include_globs,
        ),
        AuditCommand::VerifyFullRepo {
            manifest,
            reports,
            batch_id,
            skip_current_hash_check,
            receipt_out,
            json,
        } => run_full_repo_verify(
            manifest,
            reports,
            batch_id,
            skip_current_hash_check,
            receipt_out,
            json,
        ),
        AuditCommand::MergeFindings {
            reports,
            json_out,
            markdown_out,
            manifest,
            verification_receipt,
            ledger_projection_out,
            json,
        } => {
            let options = devcoordinator2_tooling::audit_findings::MergeCommandOptions {
                reports,
                json_out,
                markdown_out,
                manifest,
                verification_receipt,
                ledger_projection_out,
            };
            match devcoordinator2_tooling::audit_findings::run_merge_command(&options) {
                Ok(output) if json => match serde_json::to_string_pretty(&output.result) {
                    Ok(rendered) => {
                        println!("{rendered}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => tooling_error(&format!("cannot encode findings: {error}"), 2),
                },
                Ok(output) => {
                    let result = output.result;
                    let count =
                        |priority: &str| result.priority_counts.get(priority).copied().unwrap_or(0);
                    println!(
                        "{} unique findings from {} raw across {} reports (P0 {}, P1 {}, P2 {}, P3 {})",
                        result.unique_findings,
                        result.raw_findings,
                        result.reports_scanned,
                        count("P0"),
                        count("P1"),
                        count("P2"),
                        count("P3")
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&format!("could not merge audit reports: {error}"), 2),
            }
        }
    }
}

fn run_marker_free(command: MarkerFreeCommand) -> ExitCode {
    let result = match command {
        MarkerFreeCommand::ValidateSuite { suite_root } => {
            let root =
                suite_root.unwrap_or_else(devcoordinator2_tooling::marker_free::default_root);
            devcoordinator2_tooling::marker_free::validate_suite_result(&root)
        }
        MarkerFreeCommand::ValidateResponse {
            suite_root,
            case_id,
            response,
        } => {
            let root =
                suite_root.unwrap_or_else(devcoordinator2_tooling::marker_free::default_root);
            devcoordinator2_tooling::marker_free::validate_one_response(&root, &case_id, &response)
        }
        MarkerFreeCommand::Score {
            suite_root,
            responses,
        } => {
            let root =
                suite_root.unwrap_or_else(devcoordinator2_tooling::marker_free::default_root);
            devcoordinator2_tooling::marker_free::score_response_directory(&root, &responses)
        }
    };
    match result {
        Ok(result) => {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
            ExitCode::SUCCESS
        }
        Err(error) => tooling_error(&format!("marker-free eval error: {error}"), 2),
    }
}

fn run_ui_implementation(command: UiImplementationCommand) -> ExitCode {
    match command {
        UiImplementationCommand::Build {
            repo,
            out,
            batch_size,
            max_batch_bytes,
            include_config: _,
            no_include_config,
            include_env,
            include_generated,
            include_vendor,
            include_assets: _,
            no_include_assets,
            run_id,
            exclude_globs,
            include_files,
            include_globs,
            mockup,
            journey_files,
            split_visual_discovery,
            ui_platform,
            formal_config,
            implemented_ui_files,
            implemented_ui_overrides,
            eligibility_only,
        } => {
            let result = (|| {
                use devcoordinator2_tooling::audit_queue as queue;
                use devcoordinator2_tooling::ui_audit;
                use devcoordinator2_tooling::ui_gate::ExplicitUiBasis;
                let repo = repo
                    .canonicalize()
                    .map_err(|error| format!("Repo path is not a directory: {error}"))?;
                if implemented_ui_overrides.len() % 3 != 0 {
                    return Err(
                        "--implemented-ui-override requires PATH UI-KIND SOURCE-ANCHOR".to_owned(),
                    );
                }
                let mut evidence = std::collections::BTreeMap::new();
                for raw in implemented_ui_files {
                    let path = queue::validate_repo_relative_include(&repo, &raw)?;
                    if evidence.insert(path.clone(), None).is_some() {
                        return Err(format!("duplicate implemented UI evidence path: {path}"));
                    }
                }
                for raw in implemented_ui_overrides.chunks_exact(3) {
                    let path = queue::validate_repo_relative_include(&repo, &raw[0])?;
                    let basis = ExplicitUiBasis {
                        ui_kind: raw[1].clone(),
                        source_anchor: raw[2].clone(),
                    };
                    if evidence.insert(path.clone(), Some(basis)).is_some() {
                        return Err(format!(
                            "name each implementation path with only one evidence mode; duplicated: {path}"
                        ));
                    }
                }
                let include_files = include_files
                    .iter()
                    .map(|path| queue::validate_repo_relative_include(&repo, path))
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                let forced_mockups = mockup
                    .iter()
                    .map(|path| queue::validate_repo_relative_include(&repo, path))
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                let forced_journey_files = journey_files
                    .iter()
                    .map(|path| queue::validate_repo_relative_include(&repo, path))
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                let run_id = match run_id {
                    Some(run_id) => queue::run_id_token(&run_id)?,
                    None => random_audit_run_id()?,
                };
                let stamp = audit_archive_stamp()?;
                let out = match out {
                    Some(path) if path.is_absolute() => path,
                    Some(path) => std::env::current_dir()
                        .map_err(|error| error.to_string())?
                        .join(path),
                    None => std::env::temp_dir()
                        .join("ui-implementation-audit")
                        .join(repo.file_name().unwrap_or_default())
                        .join(format!("{stamp}-{}", &run_id[..8])),
                };
                let output_rel = queue::relative_dir_if_child(&repo, &out);
                if output_rel.as_deref() == Some("") {
                    return Err("--out cannot be the repository root; choose a dedicated audit output directory.".to_owned());
                }
                let owner = queue::ArtifactOwnership {
                    owner: ui_audit::ARTIFACT_OWNER.to_owned(),
                    marker_name: ui_audit::ARTIFACT_MARKER.to_owned(),
                    ..queue::ArtifactOwnership::default()
                };
                let mut output_rel_dirs = output_rel.into_iter().collect::<Vec<_>>();
                for owned in queue::discover_owned_output_dirs(
                    &repo,
                    include_generated,
                    include_vendor,
                    &owner,
                ) {
                    if !output_rel_dirs.contains(&owned) {
                        output_rel_dirs.push(owned);
                    }
                }
                let mut all_includes = include_files;
                all_includes.extend(evidence.keys().cloned());
                let collection = queue::CollectOptions {
                    include_config: !no_include_config,
                    include_env,
                    include_generated,
                    include_vendor,
                    include_assets: !no_include_assets,
                    exclude_globs,
                    include_files: all_includes,
                    include_globs,
                    output_rel_dirs,
                };
                let (_, gate) = ui_audit::collect_and_assess_gate(&repo, &collection, &evidence);
                if eligibility_only {
                    return Ok((gate, None));
                }
                if gate["status"] != "passed" {
                    let rejected = gate["rejected_files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| {
                            Some(format!(
                                "{}: {}",
                                item["rel_path"].as_str()?,
                                item["reason"].as_str()?
                            ))
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(format!(
                        "UI implementation audit is not applicable: {}{}",
                        gate["reason"].as_str().unwrap_or("gate failed"),
                        if rejected.is_empty() {
                            String::new()
                        } else {
                            format!("; {rejected}")
                        }
                    ));
                }
                let ui_platform = ui_platform.ok_or_else(|| {
                    "--ui-platform is required for a full UI implementation audit".to_owned()
                })?;
                let verifier_program =
                    std::env::current_exe().map_err(|error| error.to_string())?;
                let manifest = ui_audit::build(&ui_audit::BuildOptions {
                    repo,
                    out: out.clone(),
                    run_id,
                    generated_at: timestamp()?,
                    archive_stamp: stamp,
                    verifier_program,
                    batch_size,
                    max_batch_bytes,
                    collection,
                    forced_mockups,
                    forced_journey_files,
                    implementation_evidence: evidence,
                    split_visual_discovery,
                    ui_platform,
                    formal_config,
                })?;
                Ok((manifest, Some(out)))
            })();
            match result {
                Ok((gate, None)) => {
                    println!("{}", serde_json::to_string_pretty(&gate).unwrap());
                    if gate["status"] == "passed" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(devcoordinator2_tooling::ui_audit::INAPPLICABLE_EXIT)
                    }
                }
                Ok((manifest, Some(out))) => {
                    println!(
                        "Wrote {} UI implementation batches covering {} interface source files to {}",
                        manifest["batch_count"],
                        manifest["source_file_count"],
                        out.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(error) if error.contains("not applicable") => tooling_error(&error, 3),
                Err(error) => tooling_error(&error, 2),
            }
        }
        UiImplementationCommand::ImportFormal {
            audit_root,
            run_id,
            formal_report,
            journey_evidence,
            review_queue,
            manual_review,
        } => match devcoordinator2_tooling::ui_audit::import_formal_evidence(
            &devcoordinator2_tooling::ui_audit::ImportFormalOptions {
                audit_root,
                run_id,
                formal_report,
                journey_evidence,
                review_queue,
                manual_review,
            },
        ) {
            Ok(result) => {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                ExitCode::SUCCESS
            }
            Err(error) => tooling_error(&error, 2),
        },
        UiImplementationCommand::Verify {
            manifest,
            reports,
            skip_current_hash_check,
            json,
        } => match devcoordinator2_tooling::ui_audit_verify::verify(
            &manifest,
            &reports,
            skip_current_hash_check,
        ) {
            Ok(result) if json => {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                if result["ok"] == true {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            Ok(result) => {
                println!("ok: {}", result["ok"]);
                println!("run_id: {}", result["run_id"]);
                if let Some(issues) = result["issues"].as_object() {
                    for (kind, values) in issues {
                        let count = values.as_array().map_or(0, Vec::len);
                        if count > 0 {
                            println!("{kind}: {count}");
                        }
                    }
                }
                if result["ok"] == true {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            Err(error) => tooling_error(&error, 2),
        },
    }
}

fn run_test_coverage(command: TestCoverageCommand) -> ExitCode {
    match command {
        TestCoverageCommand::Build {
            repo,
            out,
            batch_size,
            max_batch_bytes,
            include_config: _,
            no_include_config,
            include_env,
            include_generated,
            include_vendor,
            include_assets,
            run_id,
            exclude_globs,
            include_files,
            include_globs,
            coverage_reports,
            assurance_input,
        } => {
            let result = (|| {
                let repo = repo
                    .canonicalize()
                    .map_err(|error| format!("Repo path is not a directory: {error}"))?;
                let run_id = match run_id {
                    Some(run_id) => devcoordinator2_tooling::audit_queue::run_id_token(&run_id)?,
                    None => random_audit_run_id()?,
                };
                let stamp = audit_archive_stamp()?;
                let out = match out {
                    Some(path) if path.is_absolute() => path,
                    Some(path) => std::env::current_dir()
                        .map_err(|error| error.to_string())?
                        .join(path),
                    None => std::env::temp_dir()
                        .join("full-repo-test-coverage-audit")
                        .join(repo.file_name().unwrap_or_default())
                        .join(format!("{stamp}-{}", &run_id[..8])),
                };
                let mut output_rel_dirs =
                    devcoordinator2_tooling::audit_queue::relative_dir_if_child(&repo, &out)
                        .into_iter()
                        .collect::<Vec<_>>();
                if output_rel_dirs == [String::new()] {
                    return Err("--out cannot be the repository root".to_owned());
                }
                let owner = devcoordinator2_tooling::audit_queue::ArtifactOwnership {
                    owner: devcoordinator2_tooling::test_coverage_audit::ARTIFACT_OWNER.to_owned(),
                    marker_name: devcoordinator2_tooling::test_coverage_audit::ARTIFACT_MARKER
                        .to_owned(),
                    ..devcoordinator2_tooling::audit_queue::ArtifactOwnership::default()
                };
                for owned in devcoordinator2_tooling::audit_queue::discover_owned_output_dirs(
                    &repo,
                    include_generated,
                    include_vendor,
                    &owner,
                ) {
                    if !output_rel_dirs.contains(&owned) {
                        output_rel_dirs.push(owned);
                    }
                }
                let include_files = include_files
                    .iter()
                    .map(|path| {
                        devcoordinator2_tooling::audit_queue::validate_repo_relative_include(
                            &repo, path,
                        )
                    })
                    .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
                let verifier = std::env::current_exe().map_err(|error| error.to_string())?;
                let manifest = devcoordinator2_tooling::test_coverage_audit::build(
                    &devcoordinator2_tooling::test_coverage_audit::BuildOptions {
                        repo,
                        out: out.clone(),
                        run_id,
                        generated_at: timestamp()?,
                        archive_stamp: stamp,
                        verifier_program: verifier,
                        batch_size,
                        max_batch_bytes,
                        collection: devcoordinator2_tooling::audit_queue::CollectOptions {
                            include_config: !no_include_config,
                            include_env,
                            include_generated,
                            include_vendor,
                            include_assets,
                            exclude_globs,
                            include_files,
                            include_globs,
                            output_rel_dirs,
                        },
                        coverage_reports,
                        assurance_input,
                    },
                )?;
                Ok((manifest, out))
            })();
            match result {
                Ok((manifest, out)) => {
                    println!(
                        "Wrote {} test coverage batches covering {} source files to {}",
                        manifest["batch_count"],
                        manifest["source_file_count"],
                        out.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 2),
            }
        }
        TestCoverageCommand::Verify {
            manifest,
            reports,
            skip_current_hash_check,
            require_assurance,
            json,
        } => match devcoordinator2_tooling::test_coverage_audit::verify(
            &manifest,
            &reports,
            skip_current_hash_check,
        ) {
            Ok(result) if json => {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                if result["ok"] == true {
                    if require_assurance && result["assurance_met"] != true {
                        ExitCode::from(3)
                    } else {
                        ExitCode::SUCCESS
                    }
                } else {
                    ExitCode::from(1)
                }
            }
            Ok(result) => {
                println!("ok: {}", result["ok"]);
                println!("run_id: {}", result["run_id"]);
                println!(
                    "coverage: {} | UI: {} | efficiency: {}",
                    result["assurance"]["coverage"],
                    result["assurance"]["ui"],
                    result["assurance"]["efficiency"]
                );
                println!("assurance_met: {}", result["assurance_met"]);
                if let Some(issues) = result["issues"].as_object() {
                    for (name, values) in issues {
                        if let Some(values) = values.as_array().filter(|values| !values.is_empty())
                        {
                            println!("{name}: {}", values.len());
                        }
                    }
                }
                if result["ok"] == true {
                    if require_assurance && result["assurance_met"] != true {
                        ExitCode::from(3)
                    } else {
                        ExitCode::SUCCESS
                    }
                } else {
                    ExitCode::from(1)
                }
            }
            Err(error) => tooling_error(&error, 2),
        },
    }
}

fn run_journey_docs(command: JourneyDocsCommand) -> ExitCode {
    match command {
        JourneyDocsCommand::Inventory { repo, json } => {
            match devcoordinator2_tooling::journey_docs::build_inventory(&repo) {
                Ok(inventory) if json => match serde_json::to_string_pretty(&inventory) {
                    Ok(value) => {
                        println!("{value}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => tooling_error(&format!("cannot encode inventory: {error}"), 2),
                },
                Ok(inventory) => {
                    print!(
                        "{}",
                        devcoordinator2_tooling::journey_docs::render_inventory(&inventory)
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => tooling_error(&error, 2),
            }
        }
        JourneyDocsCommand::Verify { report } => {
            let bytes =
                match devcoordinator2_tooling::audit_ledger::read_bytes_nofollow(&report, None) {
                    Ok(Some(bytes)) => bytes,
                    _ => {
                        println!("ERROR: report not found: {}", report.display());
                        return ExitCode::from(1);
                    }
                };
            let text = String::from_utf8_lossy(&bytes);
            let issues = devcoordinator2_tooling::journey_docs::verify_report(&text);
            if issues.is_empty() {
                println!("journey-docs audit report verified");
                ExitCode::SUCCESS
            } else {
                for issue in issues {
                    println!("ERROR: {issue}");
                }
                ExitCode::from(1)
            }
        }
    }
}

fn run_full_repo_verify(
    manifest: PathBuf,
    reports: Vec<PathBuf>,
    batch_id: Option<String>,
    skip_current_hash_check: bool,
    receipt_out: Option<PathBuf>,
    json_output: bool,
) -> ExitCode {
    let options = devcoordinator2_tooling::audit_verify::VerifyOptions {
        manifest,
        reports,
        batch_id,
        skip_current_hash_check,
        receipt_out,
    };
    match devcoordinator2_tooling::audit_verify::verify(&options) {
        Ok(run) if json_output => match serde_json::to_string_pretty(&run.result) {
            Ok(value) => {
                println!("{value}");
                if run.result.get("ok") == Some(&serde_json::json!(true)) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            Err(error) => tooling_error(&format!("cannot encode verifier result: {error}"), 2),
        },
        Ok(run) => {
            println!("Expected files: {}", run.result["expected_count"]);
            println!("Reported files: {}", run.result["reported_count"]);
            println!("Expected batches: {}", run.result["expected_batch_count"]);
            for key in [
                "missing",
                "unchecked",
                "duplicate",
                "extra",
                "missing_batch_reports",
                "unassigned_reports",
                "current_hash_mismatches",
                "effort_ledger_mismatches",
                "implementation_inventory_issues",
                "interface_inventory_issues",
                "lead_reconciliation_issues",
                "semantic_report_issues",
            ] {
                println!("{key}: {}", run.result[key].as_array().map_or(0, Vec::len));
            }
            let ok = run.result.get("ok") == Some(&serde_json::json!(true));
            println!("ok: {ok}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => tooling_error(&error, 2),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_full_repo_build(
    repo: PathBuf,
    out: Option<PathBuf>,
    batch_size: usize,
    max_batch_bytes: usize,
    include_config: bool,
    include_env: bool,
    include_generated: bool,
    include_vendor: bool,
    include_assets: bool,
    run_id: Option<String>,
    exclude_globs: Vec<String>,
    include_files: Vec<String>,
    include_globs: Vec<String>,
) -> ExitCode {
    use devcoordinator2_tooling::audit_queue as queue;
    let result = (|| {
        if batch_size < 1 || max_batch_bytes < 1 {
            return Err("batch size and maximum batch bytes must be at least 1".to_owned());
        }
        let repo = repo
            .canonicalize()
            .map_err(|error| format!("Repo path is not a directory: {error}"))?;
        let run_id = match run_id {
            Some(run_id) => queue::run_id_token(&run_id)?,
            None => random_audit_run_id()?,
        };
        let generated_at = timestamp()?;
        let archive_stamp = audit_archive_stamp()?;
        let out = match out {
            Some(out) if out.is_absolute() => out,
            Some(out) => std::env::current_dir()
                .map_err(|error| format!("cannot resolve output directory: {error}"))?
                .join(out),
            None => std::env::temp_dir()
                .join("full-repo-audit")
                .join(repo.file_name().unwrap_or_default())
                .join(format!("{archive_stamp}-{}", &run_id[..8])),
        };
        let output_rel = queue::relative_dir_if_child(&repo, &out);
        if output_rel.as_deref() == Some("") {
            return Err(
                "--out cannot be the repository root; choose a dedicated audit output directory."
                    .to_owned(),
            );
        }
        let ownership = queue::ArtifactOwnership::default();
        let mut output_rel_dirs = output_rel.into_iter().collect::<Vec<_>>();
        for owned in
            queue::discover_owned_output_dirs(&repo, include_generated, include_vendor, &ownership)
        {
            if !output_rel_dirs.contains(&owned) {
                output_rel_dirs.push(owned);
            }
        }
        let include_files = include_files
            .iter()
            .map(|path| queue::validate_repo_relative_include(&repo, path))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let collection = queue::collect_files(
            &repo,
            &queue::CollectOptions {
                include_config,
                include_env,
                include_generated,
                include_vendor,
                include_assets,
                exclude_globs,
                include_files,
                include_globs,
                output_rel_dirs,
            },
        );
        let units = queue::audit_units_for(&repo, &collection.entries, max_batch_bytes);
        let batches = queue::batch_files(&units, batch_size, max_batch_bytes)?;
        let verifier_program = std::env::current_exe()
            .map_err(|error| format!("cannot resolve tooling executable: {error}"))?;
        let manifest = queue::write_full_repo_outputs(
            &repo,
            &out,
            &collection,
            &units,
            &batches,
            &run_id,
            &queue::FullRepoOutputOptions {
                generated_at,
                archive_stamp,
                verifier_program,
                ownership,
            },
        )?;
        Ok((manifest, out))
    })();
    match result {
        Ok((manifest, out)) => {
            println!(
                "Wrote {} batches covering {} source files to {}",
                manifest["batch_count"],
                manifest["source_file_count"],
                out.display()
            );
            println!(
                "Excluded {} entries; see {}",
                manifest["excluded_file_count"],
                out.join("excluded_files.json").display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => tooling_error(&error, 2),
    }
}

fn random_audit_run_id() -> Result<String, String> {
    use std::fmt::Write as _;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| format!("cannot generate run id: {error}"))?;
    Ok(random
        .iter()
        .fold(String::with_capacity(32), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        }))
}

fn audit_archive_stamp() -> Result<String, String> {
    use time::{OffsetDateTime, macros::format_description};
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .map_err(|error| format!("cannot format audit timestamp: {error}"))
}

fn run_install(command: InstallCommand) -> ExitCode {
    use devcoordinator2_tooling::install::{self, HostRunner};
    let result = match command {
        InstallCommand::Configure {
            source_root,
            base_domain,
            admin_emails,
            client_accounts,
            client_group,
            canary,
            canary_port,
            compose_env_authorizations,
            codex_usage_accounts,
        } => (|| {
            if rustix::process::geteuid().as_raw() != 0 {
                return Err("install configure must run as root".to_owned());
            }
            let source_root = source_root
                .canonicalize()
                .map_err(|error| format!("cannot resolve source root: {error}"))?;
            let source_commit = install::validate_live_checkout(&source_root, false, &HostRunner)?;
            let accounts = client_accounts
                .split(',')
                .map(str::trim)
                .filter(|account| !account.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if accounts.is_empty() {
                return Err("--client-accounts must name at least one account".to_owned());
            }
            let (identities, edge_account) = install::ensure_identities(
                &client_group,
                &accounts,
                "devcoordinator2-edge",
                &HostRunner,
                &install::HostIdentityDirectory,
            )?;
            let compose_authorizations =
                install::compose_env_authorizations(&compose_env_authorizations, &HostRunner)?;
            let usage_sources = install::codex_usage_sources(&codex_usage_accounts, |name| {
                install::account_by_name(name)
            })?;
            let paths = install::PreparePaths::default();
            let prepared = install::prepare_instance(
                &install::PrepareRequest {
                    source_root: source_root.clone(),
                    base_domain,
                    admin_emails,
                    identities: identities.clone(),
                    edge_account,
                    system_owner: (0, 0),
                    canary,
                    canary_port,
                    compose_authorizations,
                    usage_sources,
                    paths: paths.clone(),
                },
                &HostRunner,
            )?;
            let links = install_agent_links(&source_root, &accounts, &paths.state_dir)?;
            Ok(serde_json::json!({
                "source_commit": source_commit,
                "identities": identities,
                "prepared": prepared,
                "agent_links": links,
            }))
        })(),
        InstallCommand::Preflight { source_root, fetch } => {
            install::validate_live_checkout(&source_root, fetch, &HostRunner).map(|commit| {
                serde_json::json!({
                    "source_root": source_root,
                    "source_commit": commit,
                    "clean_main": true,
                })
            })
        }
        InstallCommand::Build {
            source_root,
            manifest: manifest_path,
        } => (|| {
            if rustix::process::geteuid().as_raw() != 0 {
                return Err("install build must run as root".to_owned());
            }
            let source_root = source_root
                .canonicalize()
                .map_err(|error| format!("cannot resolve source root: {error}"))?;
            let source_commit = install::validate_live_checkout(&source_root, false, &HostRunner)?;
            let identity = install::checkout_build_identity(&source_root)?;
            let binaries = install::build_release_binaries(
                &source_root,
                &source_commit,
                &identity,
                &install::BuildTools::default(),
                &HostRunner,
            )?;
            let built_at = timestamp()?;
            let document = install::manifest(&source_root, &source_commit, &built_at, binaries)?;
            install::write_manifest(&manifest_path, &document, (0, 0))?;
            serde_json::to_value(document)
                .map_err(|error| format!("cannot encode installation manifest: {error}"))
        })(),
        InstallCommand::Verify { manifest } => {
            install::read_and_verify_manifest(&manifest, &HostRunner).and_then(|document| {
                serde_json::to_value(document)
                    .map_err(|error| format!("cannot encode installation manifest: {error}"))
            })
        }
        InstallCommand::Plan { manifest } => {
            install::read_and_verify_manifest(&manifest, &HostRunner)
                .and_then(|document| install::installation_plan(&document, &manifest))
                .and_then(|plan| {
                    serde_json::to_value(plan)
                        .map_err(|error| format!("cannot encode installation plan: {error}"))
                })
        }
        InstallCommand::Activate {
            candidate_manifest,
            transaction_dir,
            canary,
            yes,
        } => {
            if !yes {
                return tooling_error(
                    "install activate requires --yes after the reviewed cutover plan is ready",
                    2,
                );
            }
            if rustix::process::geteuid().as_raw() != 0 {
                return tooling_error("install activate must run as root", 2);
            }
            let config = devcoordinator2_tooling::cutover::HostCutoverConfig {
                candidate_manifest,
                transaction_dir: transaction_dir.unwrap_or_else(|| {
                    PathBuf::from(format!(
                        "/var/lib/devcoordinator2/cutover/activation-{}-{}",
                        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
                        std::process::id()
                    ))
                }),
                canary,
                ..Default::default()
            };
            install::read_and_verify_manifest(&config.candidate_manifest, &HostRunner).and_then(
                |candidate| {
                    let checkout_commit = install::validate_live_checkout(
                        std::path::Path::new(&candidate.source_root),
                        false,
                        &HostRunner,
                    )?;
                    if checkout_commit != candidate.source_commit {
                        return Err(
                            "candidate binaries do not match the current clean main checkout"
                                .to_owned(),
                        );
                    }
                    let control_binary = candidate
                        .binaries
                        .iter()
                        .find(|binary| binary.name == "devcoordinator2")
                        .map(|binary| PathBuf::from(&binary.path))
                        .ok_or_else(|| "candidate manifest has no control binary".to_owned())?;
                    let validated = install::validate_registered_repository_configs(
                        &config.database_path,
                        &control_binary,
                        &HostRunner,
                    )?;
                    let mut host = devcoordinator2_tooling::cutover::HostCutover::new(
                        config,
                        std::sync::Arc::new(HostRunner),
                    )?;
                    let receipt = devcoordinator2_tooling::cutover::activate(&mut host)?;
                    Ok(serde_json::json!({
                        "cutover": receipt,
                        "validated_repository_configs": validated,
                    }))
                },
            )
        }
        InstallCommand::Recover {
            transaction_dir,
            yes,
        } => {
            if transaction_dir.is_none() {
                println!(
                    "{}",
                    serde_json::json!({"ok":false,"error":{"code":"recovery_target_required","message":"Specify the in-progress installation transaction returned by activation."}})
                );
                return ExitCode::from(2);
            }
            if !yes {
                return tooling_error(
                    "install recover requires --yes to restore the captured prior installation",
                    2,
                );
            }
            if rustix::process::geteuid().as_raw() != 0 {
                return tooling_error("install recover must run as root", 2);
            }
            let config = devcoordinator2_tooling::cutover::RecoveryConfig {
                transaction_dir: transaction_dir.unwrap_or_default(),
                ..Default::default()
            };
            devcoordinator2_tooling::cutover::recover_host(&config, &HostRunner, (0, 0)).and_then(
                |receipt| {
                    serde_json::to_value(receipt)
                        .map_err(|error| format!("cannot encode recovery receipt: {error}"))
                },
            )
        }
    };
    match result {
        Ok(value) => emit_report(value, true, 1),
        Err(error) if error.starts_with("recovery target") => {
            println!(
                "{}",
                serde_json::json!({"ok":false,"error":{"code":"recovery_target_invalid","message":error}})
            );
            ExitCode::from(2)
        }
        Err(error) => tooling_error(&error, 2),
    }
}

fn install_agent_links(
    source_root: &std::path::Path,
    accounts: &[String],
    state_dir: &std::path::Path,
) -> Result<serde_json::Value, String> {
    use devcoordinator2_tooling::skill_links::{self, PolicyTarget};
    let mut skill_roots = Vec::new();
    let mut policy_targets = Vec::new();
    let mut retired = Vec::new();
    for name in accounts {
        let account = devcoordinator2_tooling::install::account_by_name(name)?;
        for agent in [".codex", ".claude"] {
            let root = account.home.join(agent);
            if !root.is_dir() {
                continue;
            }
            let skills = root.join("skills");
            devcoordinator2_tooling::install::ensure_owned_directory(
                &skills,
                0o755,
                (account.uid, account.gid),
            )?;
            let legacy = skills.join("codex-dev-coordinator");
            if devcoordinator2_tooling::install::retire_legacy_skill_link(&legacy)? {
                retired.push(legacy);
            }
            skill_roots.push(skills);
            policy_targets.push(if agent == ".codex" {
                PolicyTarget::codex(root.join("AGENTS.md"))
            } else {
                PolicyTarget::claude(root.join("CLAUDE.md"))
            });
        }
    }
    let transaction_root = state_dir.join("install-transactions");
    std::fs::create_dir_all(&transaction_root)
        .map_err(|error| format!("cannot create install transaction directory: {error}"))?;
    let nonce = transaction_nonce();
    let skill_changes = if skill_roots.is_empty() {
        0
    } else {
        skill_links::apply_skill_links(
            source_root,
            &skill_roots,
            &transaction_root.join(format!("skills-{nonce}")),
            &skill_links::SkillLinkApplyOptions {
                selected_skills: None,
                allow_noncanonical: false,
                failure_after_links: None,
            },
        )
        .map_err(|error| error.to_string())?
        .changed_entries
    };
    let policy_changes = if policy_targets.is_empty() {
        0
    } else {
        let transaction = transaction_root.join(format!("policy-{nonce}"));
        let receipt = skill_links::create_policy_plan(source_root, &transaction, &policy_targets)
            .map_err(|error| error.to_string())?;
        skill_links::apply_policy_transaction(&transaction, &receipt.digest)
            .map_err(|error| error.to_string())?
            .targets
            .len()
    };
    Ok(serde_json::json!({
        "skill_roots": skill_roots,
        "skill_changes": skill_changes,
        "policy_targets": policy_targets.len(),
        "policy_changes": policy_changes,
        "retired_legacy_links": retired,
    }))
}

fn transaction_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", std::process::id(), nanos)
}

fn source_commit_requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    arguments.next().as_deref() == Some(std::ffi::OsStr::new("--source-commit"))
        && arguments.next().is_none()
}

fn run_legacy(command: LegacyCommand) -> ExitCode {
    let now = match timestamp() {
        Ok(now) => now,
        Err(error) => return tooling_error(&error, 2),
    };
    let result = match command {
        LegacyCommand::Export(args) => {
            let options = devcoordinator2_tooling::legacy_export::ExportOptions {
                authority_db: args.authority_db,
                routes_publication: args.routes_publication,
                access_control: args.access_control,
                telegram_state: args.telegram_state,
                bugs_dir: args.bugs_dir,
                output: args.output,
            };
            devcoordinator2_tooling::legacy_export::write_export(&options, &now)
        }
        LegacyCommand::Import(args) => {
            let options = devcoordinator2_tooling::legacy_import::ImportOptions {
                export: args.export_file,
                state_dir: args.state_dir,
                bugs_dir: args.bugs_dir,
                live_containers: args.live_containers,
                current_route_map: args.current_route_map,
                routes_path: args.routes_path,
                base_domain: args.base_domain,
                prune_missing_install_fixtures: args.prune_missing_install_fixtures,
                dry_run: args.dry_run,
            };
            devcoordinator2_tooling::legacy_import::run(&options, &now)
        }
    };
    match result {
        Ok(value) => emit_report(value, true, 1),
        Err(error) => tooling_error(&error, 2),
    }
}

fn run_planning(command: PlanningCommand) -> ExitCode {
    let PlanningCommand::InspectBackup {
        transaction_dir,
        repository_id,
        task_id,
    } = command;
    let request = devcoordinator2_tooling::planning_backup::InspectRequest {
        transaction_dir,
        repository_id,
        task_ids: task_id,
    };
    match devcoordinator2_tooling::planning_backup::inspect(&request) {
        Ok(result) => emit_report(json!(result), true, 0),
        Err(error) => tooling_error(&error, 2),
    }
}

fn run_decision(command: DecisionCommand) -> ExitCode {
    let DecisionCommand::Import(args) = command;
    let source = args
        .file
        .unwrap_or_else(|| args.path.join("DecisionHistory.md"));
    let options = devcoordinator2_tooling::decision_import::ImportOptions {
        repository: args.path,
        source,
        socket: args
            .socket
            .unwrap_or_else(devcoordinator2_tooling::instance::socket_path),
        dry_run: args.dry_run,
        aspect_overrides: args.aspect_overrides,
    };
    let entries = match devcoordinator2_tooling::decision_import::load(&options) {
        Ok(entries) if !entries.is_empty() => entries,
        Ok(_) => return tooling_error("no `## REF — title` decision sections were found", 1),
        Err(error) => return tooling_error(&error, 2),
    };
    if options.dry_run {
        return emit_report(
            serde_json::json!({
                "dry_run": true,
                "entries": entries,
                "recorded": 0,
            }),
            true,
            1,
        );
    }
    match devcoordinator2_tooling::decision_import::import_with(
        &entries,
        &options.repository,
        |params| devcoordinator2_tooling::decision_import::call(&options.socket, params),
    ) {
        Ok(counts) => emit_report(
            serde_json::json!({
                "imported": counts.imported,
                "already_present": counts.skipped,
                "failed": counts.failed,
            }),
            counts.failed == 0,
            1,
        ),
        Err(error) => tooling_error(&error, 2),
    }
}

fn timestamp() -> Result<String, String> {
    use time::{OffsetDateTime, macros::format_description};
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .map_err(|error| format!("cannot format current time: {error}"))
}

fn tooling_error(message: &str, code: u8) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({
            "ok": false,
            "error": {"code": "tooling_failed", "message": message}
        })
    );
    ExitCode::from(code)
}

fn selected_skills(values: Vec<String>) -> Option<Vec<String>> {
    (!values.is_empty()).then_some(values)
}

fn run_skill_links(command: SkillLinksCommand) -> ExitCode {
    use devcoordinator2_tooling::skill_links;

    let result = match command {
        SkillLinksCommand::Plan(selection) => skill_links::build_skill_link_plan(
            &selection.repo_root,
            &selection.target_root,
            selected_skills(selection.skill).as_deref(),
        )
        .map(|plan| (plan.to_json(), true)),
        SkillLinksCommand::Verify(selection) => skill_links::verify_skill_links(
            &selection.repo_root,
            &selection.target_root,
            selected_skills(selection.skill).as_deref(),
        )
        .map(|verification| {
            let mut value = verification.plan.to_json();
            if let Some(object) = value.as_object_mut() {
                object.insert("failures".into(), serde_json::json!(verification.failures));
            }
            (value, verification.is_verified())
        }),
        SkillLinksCommand::Apply {
            selection,
            transaction_dir,
            allow_noncanonical,
        } => {
            let options = skill_links::SkillLinkApplyOptions {
                selected_skills: selected_skills(selection.skill),
                allow_noncanonical,
                failure_after_links: None,
            };
            skill_links::apply_skill_links(
                &selection.repo_root,
                &selection.target_root,
                &transaction_dir,
                &options,
            )
            .map(|transaction| {
                (
                    serde_json::json!({
                        "status": transaction.status,
                        "transaction_dir": transaction.transaction_dir,
                        "changed_entries": transaction.changed_entries,
                        "journal": transaction.document,
                    }),
                    true,
                )
            })
        }
        SkillLinksCommand::Rollback {
            transaction_dir,
            force,
        } => skill_links::rollback_skill_links(&transaction_dir, force).map(|transaction| {
            (
                serde_json::json!({
                    "status": transaction.status,
                    "transaction_dir": transaction.transaction_dir,
                    "changed_entries": transaction.changed_entries,
                    "journal": transaction.document,
                }),
                true,
            )
        }),
    };
    match result {
        Ok((value, verified)) => emit_report(value, verified, 1),
        Err(error) => link_error(error),
    }
}

fn run_policy(command: PolicyCommand) -> ExitCode {
    use devcoordinator2_tooling::skill_links::{self, PolicyTarget};

    let result = match command {
        PolicyCommand::Plan {
            repo_root,
            transaction_dir,
            codex_target,
            claude_target,
            claude_windows_import_wrapper_target,
        } => {
            let targets = codex_target
                .into_iter()
                .map(PolicyTarget::codex)
                .chain(claude_target.into_iter().map(PolicyTarget::claude))
                .chain(
                    claude_windows_import_wrapper_target
                        .into_iter()
                        .map(PolicyTarget::claude_windows_wrapper),
                )
                .collect::<Vec<_>>();
            skill_links::create_policy_plan(&repo_root, &transaction_dir, &targets).map(|receipt| {
                let mut value = receipt.plan.to_json();
                if let Some(object) = value.as_object_mut() {
                    object.insert("plan_sha256".into(), receipt.digest.into());
                }
                value
            })
        }
        PolicyCommand::Apply {
            transaction_dir,
            plan_digest,
        } => skill_links::apply_policy_transaction(&transaction_dir, &plan_digest)
            .map(|result| result.to_json()),
        PolicyCommand::Verify {
            transaction_dir,
            plan_digest,
        } => skill_links::verify_policy_transaction(&transaction_dir, &plan_digest)
            .map(|result| result.to_json()),
        PolicyCommand::Rollback {
            transaction_dir,
            plan_digest,
        } => skill_links::rollback_policy_transaction(&transaction_dir, &plan_digest)
            .map(|result| result.to_json()),
    };
    match result {
        Ok(value) => emit_report(value, true, 1),
        Err(error) => link_error(error),
    }
}

fn link_error(error: devcoordinator2_tooling::skill_links::LinkError) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({
            "ok": false,
            "error": {"code": error.kind.as_str(), "message": error.to_string()}
        })
    );
    ExitCode::from(2)
}

fn check_report(
    result: Result<
        devcoordinator2_tooling::repository_checks::CheckReport,
        devcoordinator2_tooling::repository_checks::CheckError,
    >,
) -> ExitCode {
    match result {
        Ok(report) => emit_report(report.to_json(), report.is_clean(), 1),
        Err(error) => check_error(error),
    }
}

fn check_error(error: devcoordinator2_tooling::repository_checks::CheckError) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::json!({
            "ok": false,
            "error": {"code": error.kind.as_str(), "message": error.to_string()}
        })
    );
    ExitCode::from(2)
}

fn emit_report(value: serde_json::Value, clean: bool, finding_exit: i32) -> ExitCode {
    match serde_json::to_string_pretty(&value) {
        Ok(rendered) => println!("{rendered}"),
        Err(error) => {
            eprintln!("cannot encode check result: {error}");
            return ExitCode::from(2);
        }
    }
    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(u8::try_from(finding_exit).unwrap_or(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ported_command_family_parses_without_python_wrappers() {
        let formal = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "formal-ui",
            "review",
            "--report",
            "/tmp/report.json",
            "--queue",
            "/tmp/review-queue.json",
            "--decisions",
            "/tmp/decisions.json",
            "--out",
            "/tmp/manual-review.json",
        ])
        .expect("formal UI review command");
        assert!(matches!(
            formal.command,
            Command::FormalUi {
                command: FormalUiCommand::Review {
                    decisions: Some(_),
                    ..
                }
            }
        ));
        let formal_verify = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "formal-ui",
            "verify",
            "--config",
            "/tmp/formal.json",
            "--json-out",
            "/tmp/report.json",
        ])
        .expect("formal UI verifier wrapper");
        assert!(matches!(
            formal_verify.command,
            Command::FormalUi {
                command: FormalUiCommand::Verify { arguments }
            } if arguments.len() == 4
        ));
        let formal_self_test = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "formal-ui",
            "self-test",
            "--workspace-parent",
            "/var/tmp",
            "--keep",
        ])
        .expect("formal UI Rust self-test command");
        assert!(matches!(
            formal_self_test.command,
            Command::FormalUi {
                command: FormalUiCommand::SelfTest { keep: true, .. }
            }
        ));

        let audit = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "merge-findings",
            "--reports",
            "/tmp/audit/reports",
            "--manifest",
            "/tmp/audit/manifest.json",
            "--json",
        ])
        .expect("audit findings command");
        assert!(matches!(
            audit.command,
            Command::Audit {
                command: AuditCommand::MergeFindings { json: true, .. }
            }
        ));
        let marker_free = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "marker-free",
            "score",
            "--responses",
            "/tmp/responses",
        ])
        .expect("marker-free scorer command");
        assert!(matches!(
            marker_free.command,
            Command::Audit {
                command: AuditCommand::MarkerFree {
                    command: MarkerFreeCommand::Score { .. }
                }
            }
        ));
        let journey_inventory = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "journey-docs",
            "inventory",
            "--repo",
            "/repo",
            "--json",
        ])
        .expect("journey docs inventory command");
        assert!(matches!(
            journey_inventory.command,
            Command::Audit {
                command: AuditCommand::JourneyDocs {
                    command: JourneyDocsCommand::Inventory { json: true, .. }
                }
            }
        ));
        let coverage_build = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "test-coverage",
            "build",
            "--repo",
            "/repo",
            "--out",
            "/tmp/coverage",
            "--coverage-report",
            "/tmp/lcov.info",
        ])
        .expect("test coverage build command");
        assert!(matches!(
            coverage_build.command,
            Command::Audit {
                command: AuditCommand::TestCoverage {
                    command: TestCoverageCommand::Build { .. }
                }
            }
        ));
        let ui_build = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "ui-implementation",
            "build",
            "--repo",
            "/repo",
            "--implemented-ui-override",
            "src/surface.canvas",
            "canvas_kit::Surface",
            "build_surface",
            "--ui-platform",
            "web",
        ])
        .expect("UI implementation build command");
        assert!(matches!(
            ui_build.command,
            Command::Audit {
                command: AuditCommand::UiImplementation {
                    command: UiImplementationCommand::Build { .. }
                }
            }
        ));
        let ui_import = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "ui-implementation",
            "import-formal",
            "--audit-root",
            "/tmp/ui-audit",
            "--run-id",
            "run-1",
            "--formal-report",
            "/tmp/ui-audit/formal.json",
            "--journey-evidence",
            "/tmp/ui-audit/journey.json",
            "--review-queue",
            "/tmp/ui-audit/queue.json",
            "--manual-review",
            "/tmp/ui-audit/review.json",
        ])
        .expect("UI formal evidence import command");
        assert!(matches!(
            ui_import.command,
            Command::Audit {
                command: AuditCommand::UiImplementation {
                    command: UiImplementationCommand::ImportFormal { .. }
                }
            }
        ));
        let ui_verify = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "ui-implementation",
            "verify",
            "--manifest",
            "/tmp/ui-audit/manifest.json",
            "--reports",
            "/tmp/ui-audit/reports",
            "--json",
        ])
        .expect("UI audit verifier command");
        assert!(matches!(
            ui_verify.command,
            Command::Audit {
                command: AuditCommand::UiImplementation {
                    command: UiImplementationCommand::Verify { json: true, .. }
                }
            }
        ));
        let build = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "build-full-repo",
            "--repo",
            "/repo",
            "--out",
            "/tmp/audit",
            "--no-include-config",
            "--include-assets",
            "--run-id",
            "run-1234",
        ])
        .expect("audit queue command");
        assert!(matches!(
            build.command,
            Command::Audit {
                command: AuditCommand::BuildFullRepo {
                    no_include_config: true,
                    include_assets: true,
                    ..
                }
            }
        ));
        let verify = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "audit",
            "verify-full-repo",
            "--manifest",
            "/tmp/audit/manifest.json",
            "--reports",
            "/tmp/audit/reports",
            "--batch-id",
            "batch_001",
            "--json",
        ])
        .expect("audit verifier command");
        assert!(matches!(
            verify.command,
            Command::Audit {
                command: AuditCommand::VerifyFullRepo {
                    batch_id: Some(_),
                    json: true,
                    ..
                }
            }
        ));

        let check = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "check",
            "repository-freshness",
            "--root",
            "/repo",
            "--remote",
            "upstream",
            "--branch",
            "main",
        ])
        .expect("freshness command");
        assert!(matches!(
            check.command,
            Command::Check {
                command: CheckCommand::RepositoryFreshness { .. }
            }
        ));
        let public_artifacts = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "check",
            "public-artifacts",
            "--repo",
            "/repo",
            "--json",
        ])
        .expect("public artifact guard command");
        assert!(matches!(
            public_artifacts.command,
            Command::Check {
                command: CheckCommand::PublicArtifacts { json: true, .. }
            }
        ));

        let links = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "skills",
            "links",
            "verify",
            "--repo-root",
            "/repo",
            "--target-root",
            "/runtime/skills",
            "--skill",
            "dev-coordinator",
        ])
        .expect("skill verification command");
        assert!(matches!(
            links.command,
            Command::Skills {
                command: SkillsCommand::Links {
                    command: SkillLinksCommand::Verify(_)
                }
            }
        ));

        let policy = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "skills",
            "policy",
            "plan",
            "--repo-root",
            "/repo",
            "--transaction-dir",
            "/state/transaction",
            "--codex-target",
            "/runtime/AGENTS.md",
        ])
        .expect("policy plan command");
        assert!(matches!(
            policy.command,
            Command::Skills {
                command: SkillsCommand::Policy {
                    command: PolicyCommand::Plan { .. }
                }
            }
        ));
        let skill_self_test = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "skills",
            "self-test",
            "dev-coordinator",
            "--source-root",
            "/repo",
            "--control-binary",
            "/tmp/devcoordinator2",
        ])
        .expect("dev-coordinator skill self-test command");
        assert!(matches!(
            skill_self_test.command,
            Command::Skills {
                command: SkillsCommand::SelfTest {
                    command: SkillSelfTestCommand::DevCoordinator { .. }
                }
            }
        ));
        let audit_self_test = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "skills",
            "self-test",
            "audit-tooling",
            "--source-root",
            "/repo",
            "--cargo-target-dir",
            "/tmp/cargo-target",
        ])
        .expect("audit tooling self-test command");
        assert!(matches!(
            audit_self_test.command,
            Command::Skills {
                command: SkillsCommand::SelfTest {
                    command: SkillSelfTestCommand::AuditTooling { .. }
                }
            }
        ));
        let skill_validation = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "skills",
            "validate",
            "plan",
            "--root",
            "/repo",
            "--run-id",
            "shape-only",
        ])
        .expect("Rust six-skill validation plan command");
        assert!(matches!(
            skill_validation.command,
            Command::Skills {
                command: SkillsCommand::Validate {
                    command: SkillValidationCommand::Plan { .. }
                }
            }
        ));

        let legacy = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "legacy",
            "export",
            "--authority-db",
            "/legacy/authority.sqlite3",
            "--out",
            "/tmp/export.json",
        ])
        .expect("legacy export command");
        assert!(matches!(
            legacy.command,
            Command::Legacy {
                command: LegacyCommand::Export(_)
            }
        ));

        let decision = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "decision",
            "import",
            "--path",
            "/repo",
            "--dry-run",
        ])
        .expect("decision import command");
        assert!(matches!(
            decision.command,
            Command::Decision {
                command: DecisionCommand::Import(_)
            }
        ));

        let planning = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "planning",
            "inspect-backup",
            "--transaction-dir",
            "/saved/activation",
            "--repository-id",
            "r946ed77e45b31d74",
            "--task-id",
            "p9966151ec04cd9cf",
        ])
        .expect("read-only planning backup inspection");
        assert!(matches!(planning.command, Command::Planning {
            command: PlanningCommand::InspectBackup { task_id, .. }
        } if task_id == ["p9966151ec04cd9cf"]));

        let install = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "install",
            "preflight",
            "--source-root",
            "/repo",
        ])
        .expect("install preflight command");
        assert!(matches!(
            install.command,
            Command::Install {
                command: InstallCommand::Preflight { .. }
            }
        ));
        let configure = Cli::try_parse_from([
            "devcoordinator2-tooling",
            "install",
            "configure",
            "--base-domain",
            "example.test",
            "--admin-emails",
            "owner@example.test",
            "--client-accounts",
            "developer",
        ])
        .expect("install configuration command");
        assert!(matches!(
            configure.command,
            Command::Install {
                command: InstallCommand::Configure { .. }
            }
        ));
        let activate =
            Cli::try_parse_from(["devcoordinator2-tooling", "install", "activate", "--yes"])
                .expect("install activation command");
        assert!(matches!(
            activate.command,
            Command::Install {
                command: InstallCommand::Activate { yes: true, .. }
            }
        ));
        let recover =
            Cli::try_parse_from(["devcoordinator2-tooling", "install", "recover", "--yes"])
                .expect("install recovery command");
        assert!(matches!(
            recover.command,
            Command::Install {
                command: InstallCommand::Recover { yes: true, .. }
            }
        ));
    }
}
