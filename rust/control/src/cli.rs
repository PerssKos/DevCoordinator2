//! Command-line grammar, protocol-v2 invocation mapping, and response output.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use devcoordinator2_api::{
    CliDispatch, ClientContext, ClientKind, ProtocolError, ResponseEnvelope, operation,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

#[path = "cli_glossary.rs"]
mod glossary_cli;
use glossary_cli::GlossaryCommand;

#[path = "cli_configuration.rs"]
mod configuration_cli;
use configuration_cli::ConfigCommand;

#[path = "cli_review.rs"]
mod review_cli;
use review_cli::ReviewCommand;

#[path = "cli_work_context.rs"]
mod work_context_cli;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Json,
    Human,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ClientArg {
    Codex,
    Claude,
    Cursor,
    Antigravity,
    Human,
    Edge,
    #[default]
    Other,
}

impl From<ClientArg> for ClientKind {
    fn from(value: ClientArg) -> Self {
        match value {
            ClientArg::Codex => Self::Codex,
            ClientArg::Claude => Self::Claude,
            ClientArg::Cursor => Self::Cursor,
            ClientArg::Antigravity => Self::Antigravity,
            ClientArg::Human => Self::Human,
            ClientArg::Edge => Self::Edge,
            ClientArg::Other => Self::Other,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "devcoordinator2",
    version,
    about = "Server-wide development coordinator",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Json)]
    pub format: OutputFormat,
    #[arg(long, global = true, value_enum, default_value_t = ClientArg::Other)]
    pub client: ClientArg,
    #[arg(long, global = true)]
    pub session: Option<String>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    pub fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self.command {
            Command::Daemon => Ok(Invocation::Daemon),
            Command::Mcp => Ok(Invocation::Mcp),
            Command::Ping => remote("ping", json!({})),
            Command::Test { command } => command.into_invocation(),
            Command::Deployment { command } => command.into_invocation(),
            Command::Health { command } => command.into_invocation(),
            Command::Bug { command } => Ok(Invocation::OfflineBug {
                action: command.into_action(),
            }),
            Command::Telegram { command } => command.into_invocation(),
            Command::Event { command } => command.into_invocation(),
            Command::Repository { command } => command.into_invocation(),
            Command::Plan { command } => command.into_invocation(),
            Command::Task { command } => command.into_invocation(),
            Command::Release { command } => command.into_invocation(),
            Command::Decision { command } => command.into_invocation(),
            Command::Review { command } => command.into_invocation(),
            Command::Glossary { command } => command.into_invocation(),
            Command::Config { command } => command.into_invocation(),
        }
    }
}

#[derive(Debug)]
pub enum Invocation {
    Daemon,
    Mcp,
    Remote {
        operation: &'static str,
        params: Value,
    },
    OfflineBug {
        action: OfflineBugAction,
    },
    TestEvent {
        status: TestEventStatus,
    },
    ArtifactMaterialize {
        path: String,
        run_id: String,
        check: String,
        artifacts: Vec<String>,
        destination: PathBuf,
    },
}

#[derive(Debug)]
pub enum OfflineBugAction {
    Report {
        component: String,
        summary: String,
        expected: String,
        actual: String,
        steps: String,
        run_id: Option<String>,
        deployment_id: Option<String>,
    },
    List,
    Close {
        bug_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum TestEventStatus {
    Passed,
    Failed,
    Unsafe,
}

impl TestEventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unsafe => "unsafe",
        }
    }
}

#[derive(Debug, Error)]
pub enum CliValidationError {
    #[error("{0}")]
    Invalid(String),
    #[error("cannot make path absolute: {0}")]
    Path(#[source] io::Error),
    #[error("{operation} parameters are invalid: {source}")]
    Contract {
        operation: &'static str,
        #[source]
        source: ProtocolError,
    },
    #[error("CLI route {0} is absent from the operation registry")]
    MissingOperation(&'static str),
    #[error("operation {0} has no protocol CLI route in the operation registry")]
    MissingCliRoute(&'static str),
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon,
    Mcp,
    Ping,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Glossary {
        #[command(subcommand)]
        command: GlossaryCommand,
    },
    Test {
        #[command(subcommand)]
        command: TestCommand,
    },
    Deployment {
        #[command(subcommand)]
        command: DeploymentCommand,
    },
    Health {
        #[command(subcommand)]
        command: HealthCommand,
    },
    Bug {
        #[command(subcommand)]
        command: BugCommand,
    },
    Telegram {
        #[command(subcommand)]
        command: TelegramCommand,
    },
    Event {
        #[command(subcommand)]
        command: EventCommand,
    },
    Repository {
        #[command(subcommand)]
        command: RepositoryCommand,
    },
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
}

#[derive(Debug, Args)]
struct PathArg {
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

impl PathArg {
    fn absolute(&self) -> Result<String, CliValidationError> {
        let path = match &self.path {
            Some(path) => path.clone(),
            None => std::env::current_dir().map_err(CliValidationError::Path)?,
        };
        absolute_path(path)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ValidationTierArg {
    Development,
    #[value(name = "pre-merge")]
    PreMerge,
    Release,
}

impl ValidationTierArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::PreMerge => "pre-merge",
            Self::Release => "release",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum LogPhaseArg {
    Executor,
    Check,
    Discovery,
    Case,
}

impl LogPhaseArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Executor => "executor",
            Self::Check => "check",
            Self::Discovery => "discovery",
            Self::Case => "case",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum LogStreamArg {
    Stdout,
    Stderr,
}

impl LogStreamArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Args)]
struct OptionalLogSelector {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    check: Option<String>,
    #[arg(long, value_enum)]
    phase: Option<LogPhaseArg>,
    #[arg(long = "case")]
    case_name: Option<String>,
    #[arg(long, value_enum)]
    stream: Option<LogStreamArg>,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Debug, Args)]
struct RequiredLogSelector {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    check: Option<String>,
    #[arg(long, value_enum)]
    phase: LogPhaseArg,
    #[arg(long = "case")]
    case_name: Option<String>,
    #[arg(long, value_enum)]
    stream: LogStreamArg,
    #[arg(long)]
    cursor: Option<String>,
}

#[derive(Debug, Subcommand)]
enum TestCommand {
    Start(TestStartArgs),
    Retry(TestRetryArgs),
    Status(PathArg),
    History {
        #[command(flatten)]
        path: PathArg,
        #[arg(long)]
        before: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u16,
    },
    Log {
        #[command(subcommand)]
        command: TestLogCommand,
    },
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommand,
    },
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    Stop(TestStopArgs),
    Event {
        #[arg(value_enum)]
        status: TestEventStatus,
    },
    List {
        #[arg(long)]
        after_worktree_id: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=50))]
        limit: Option<u16>,
    },
    Capacity {
        #[command(subcommand)]
        command: CapacityCommand,
    },
}

#[derive(Debug, Args)]
struct TestStartArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long = "test")]
    test_name: Option<String>,
    #[arg(long = "check", action = clap::ArgAction::Append)]
    checks: Vec<String>,
    #[arg(long, value_enum, default_value_t = ValidationTierArg::Release)]
    tier: ValidationTierArg,
}

#[derive(Debug, Args)]
struct TestRetryArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long = "test")]
    test_name: Option<String>,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    check: String,
}

#[derive(Debug, Args)]
struct TestStopArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Debug, Subcommand)]
enum TestLogCommand {
    Catalog {
        #[command(flatten)]
        selector: OptionalLogSelector,
        #[arg(long, default_value_t = 100)]
        limit: u16,
    },
    Tail {
        #[command(flatten)]
        selector: RequiredLogSelector,
        #[arg(long, default_value_t = 50)]
        lines: u16,
        #[arg(long, default_value_t = 32_768)]
        max_bytes: u32,
    },
    Search {
        #[command(flatten)]
        selector: RequiredLogSelector,
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = 20)]
        max_matches: u16,
        #[arg(long, default_value_t = 2)]
        context_lines: u16,
        #[arg(long, default_value_t = 32_768)]
        max_bytes: u32,
    },
    Range {
        #[command(flatten)]
        selector: RequiredLogSelector,
        #[arg(long)]
        line_start: Option<u64>,
        #[arg(long)]
        line_end: Option<u64>,
        #[arg(long)]
        byte_start: Option<u64>,
        #[arg(long)]
        byte_end: Option<u64>,
        #[arg(long, default_value_t = 49_152)]
        max_bytes: u32,
    },
    FailureContext {
        #[command(flatten)]
        selector: RequiredLogSelector,
        #[arg(long, default_value_t = 20)]
        limit: u16,
        #[arg(long, default_value_t = 2)]
        context_lines: u16,
        #[arg(long, default_value_t = 32_768)]
        max_bytes: u32,
    },
    Retention {
        #[command(subcommand)]
        command: RetentionCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RetentionCommand {
    Show,
    Set {
        #[arg(long)]
        max_age_seconds: u32,
        #[arg(long)]
        case_depth: u16,
    },
}

#[derive(Debug, Subcommand)]
enum EvidenceCommand {
    Lookup {
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        image_id: Option<String>,
        #[arg(long)]
        worktree_id: Option<String>,
    },
    Show(EvidenceReferenceArgs),
    Image(EvidenceImageArgs),
    Feedback {
        #[command(subcommand)]
        command: FeedbackCommand,
    },
}

#[derive(Debug, Args)]
struct EvidenceReferenceArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
}

#[derive(Debug, Args)]
struct EvidenceImageArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    image_id: String,
    #[arg(long, default_value_t = 0)]
    offset: u32,
    #[arg(long, default_value_t = 184_320)]
    max_bytes: u32,
}

#[derive(Debug, Subcommand)]
enum FeedbackCommand {
    Create(FeedbackCreateArgs),
    Reply(FeedbackReplyArgs),
    Edit(FeedbackEditArgs),
    State(FeedbackStateArgs),
    Delete(FeedbackDeleteArgs),
}

#[derive(Debug, Args)]
struct FeedbackCreateArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    image_id: String,
    #[arg(long)]
    body: String,
    #[arg(long)]
    marks_json: String,
}

#[derive(Debug, Args)]
struct FeedbackReplyArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    feedback_id: String,
    #[arg(long)]
    body: String,
}

#[derive(Debug, Args)]
struct FeedbackEditArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    feedback_id: String,
    #[arg(long)]
    comment_id: String,
    #[arg(long)]
    body: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum FeedbackStateArg {
    Open,
    Resolved,
}

impl FeedbackStateArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Debug, Args)]
struct FeedbackStateArgs {
    /// Argparse accepted an optional PATH before the required STATE. Clap
    /// deliberately rejects that ambiguous positional shape, so retain the
    /// established one-or-two-value syntax and disambiguate it locally.
    #[arg(value_names = ["PATH", "STATE"], num_args = 1..=2)]
    path_and_state: Vec<String>,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    feedback_id: String,
}

#[derive(Debug, Args)]
struct FeedbackDeleteArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    feedback_id: String,
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    Catalog(ArtifactCatalogArgs),
    File(ArtifactFileArgs),
    Materialize(ArtifactMaterializeArgs),
}

#[derive(Debug, Args)]
struct ArtifactCatalogArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    check: String,
    #[arg(long)]
    artifact: Option<String>,
    #[arg(long)]
    manifest_sha256: Option<String>,
    #[arg(long, default_value_t = 0)]
    offset: u32,
    #[arg(long, default_value_t = 100)]
    limit: u16,
}

#[derive(Debug, Args)]
struct ArtifactFileArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    check: String,
    #[arg(long)]
    artifact: String,
    #[arg(long = "file")]
    file_name: String,
    #[arg(long)]
    manifest_sha256: String,
    #[arg(long, default_value_t = 0)]
    offset: u64,
    #[arg(long, default_value_t = 184_320)]
    max_bytes: u32,
}

#[derive(Debug, Args)]
struct ArtifactMaterializeArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    check: String,
    #[arg(long = "artifact", action = clap::ArgAction::Append)]
    artifacts: Vec<String>,
    #[arg(long)]
    destination: PathBuf,
}

#[derive(Debug, Subcommand)]
enum CapacityCommand {
    Show,
    Set { cap: u16 },
    Clear,
}

#[derive(Debug, Subcommand)]
enum DeploymentCommand {
    List {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    Apply(DeploymentReferenceArgs),
    Preflight(DeploymentReferenceArgs),
    Status(DeploymentReferenceArgs),
    Rollback(DeploymentReferenceArgs),
    Start(DeploymentControlArgs),
    Stop(DeploymentControlArgs),
    Restart(DeploymentControlArgs),
    Logs(DeploymentLogsArgs),
    Remove(DeploymentRemoveArgs),
    SetDomain(SetDomainArgs),
}

#[derive(Debug, Args)]
struct DeploymentSelector {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    deployment_id: Option<String>,
}

impl DeploymentSelector {
    fn params(&self) -> Result<Map<String, Value>, CliValidationError> {
        let mut params = Map::new();
        params.insert("path".to_owned(), Value::String(self.path.absolute()?));
        insert_option(&mut params, "name", self.name.clone());
        insert_option(&mut params, "deployment_id", self.deployment_id.clone());
        Ok(params)
    }
}

#[derive(Debug, Args)]
struct DeploymentReferenceArgs {
    #[command(flatten)]
    selector: DeploymentSelector,
}

#[derive(Debug, Args)]
struct DeploymentControlArgs {
    #[command(flatten)]
    selector: DeploymentSelector,
    #[arg(long)]
    component: Option<String>,
}

#[derive(Debug, Args)]
struct DeploymentLogsArgs {
    #[command(flatten)]
    selector: DeploymentSelector,
    #[arg(long)]
    component: Option<String>,
    #[arg(long, default_value_t = 200)]
    tail_lines: u16,
}

#[derive(Debug, Args)]
struct DeploymentRemoveArgs {
    #[command(flatten)]
    selector: DeploymentSelector,
    #[arg(long)]
    delete_data: bool,
}

#[derive(Debug, Args)]
struct SetDomainArgs {
    #[arg(long)]
    deployment_id: String,
    #[arg(long)]
    domain: Option<String>,
    #[arg(long)]
    clear: bool,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    component: Option<String>,
    #[arg(long, conflicts_with = "authenticated")]
    public: bool,
    #[arg(long, conflicts_with = "public")]
    authenticated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum MetricSubjectKindArg {
    Host,
    Repository,
    Component,
    Container,
    Test,
    Daemon,
    Other,
    Worktree,
    Deployment,
}

impl MetricSubjectKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Repository => "repository",
            Self::Component => "component",
            Self::Container => "container",
            Self::Test => "test",
            Self::Daemon => "daemon",
            Self::Other => "other",
            Self::Worktree => "worktree",
            Self::Deployment => "deployment",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum MetricNameArg {
    CpuPercent,
    MemoryBytes,
    Pids,
    StorageBytes,
    MemoryUsed,
    #[value(name = "load_1")]
    Load1,
    PgConnections,
    PgWalBytes,
    PgTempBytes,
    PgDatabaseBytes,
    IoRead,
    IoWrite,
}

impl MetricNameArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::CpuPercent => "cpu_percent",
            Self::MemoryBytes => "memory_bytes",
            Self::Pids => "pids",
            Self::StorageBytes => "storage_bytes",
            Self::MemoryUsed => "memory_used",
            Self::Load1 => "load1",
            Self::PgConnections => "pg_connections",
            Self::PgWalBytes => "pg_wal_bytes",
            Self::PgTempBytes => "pg_temp_bytes",
            Self::PgDatabaseBytes => "pg_database_bytes",
            Self::IoRead => "io_read",
            Self::IoWrite => "io_write",
        }
    }
}

#[derive(Debug, Subcommand)]
enum HealthCommand {
    Containers,
    Summary,
    Repositories,
    Repository(PathArg),
    History {
        #[arg(long, value_enum)]
        subject_kind: MetricSubjectKindArg,
        #[arg(long)]
        subject_id: String,
        #[arg(long, value_enum)]
        metric: MetricNameArg,
        #[arg(long, default_value_t = 60)]
        minutes: u32,
    },
}

#[derive(Debug, Subcommand)]
enum BugCommand {
    Report {
        #[arg(long)]
        component: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        expected: String,
        #[arg(long)]
        actual: String,
        #[arg(long)]
        steps: String,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        deployment_id: Option<String>,
    },
    List,
    Close {
        bug_id: String,
    },
}

impl BugCommand {
    fn into_action(self) -> OfflineBugAction {
        match self {
            Self::Report {
                component,
                summary,
                expected,
                actual,
                steps,
                run_id,
                deployment_id,
            } => OfflineBugAction::Report {
                component,
                summary,
                expected,
                actual,
                steps,
                run_id,
                deployment_id,
            },
            Self::List => OfflineBugAction::List,
            Self::Close { bug_id } => OfflineBugAction::Close { bug_id },
        }
    }
}

#[derive(Debug, Subcommand)]
enum TelegramCommand {
    List,
    Link {
        #[arg(long)]
        code: String,
        #[arg(long)]
        email: String,
    },
    Subscribe {
        #[arg(long)]
        chat_id: i64,
        #[arg(long)]
        scope: String,
    },
    Unsubscribe {
        #[arg(long)]
        chat_id: i64,
        #[arg(long)]
        scope: String,
    },
}

#[derive(Debug, Subcommand)]
enum EventCommand {
    Wait {
        #[arg(long)]
        cursor: Option<u64>,
        #[arg(long = "filter", required = true)]
        filters: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: u16,
    },
}

#[derive(Debug, Subcommand)]
enum RepositoryCommand {
    List {
        #[arg(long = "all")]
        include_archived: bool,
    },
    Status(PathArg),
    Register(PathArg),
    Archive {
        repository_id: String,
        #[arg(long = "into")]
        merged_into_repository_id: String,
        #[arg(long)]
        note: String,
    },
    Unarchive {
        repository_id: String,
        #[arg(long)]
        note: String,
    },
}

#[derive(Debug, Subcommand)]
enum PlanCommand {
    Overview {
        #[command(flatten)]
        path: PathArg,
        #[arg(long = "all")]
        all_repositories: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum TaskKindArg {
    Goal,
    Stub,
    Improvement,
    UserFeedback,
}

impl TaskKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Stub => "stub",
            Self::Improvement => "improvement",
            Self::UserFeedback => "user_feedback",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum TaskStatusArg {
    Planned,
    InProgress,
    Done,
    Dropped,
}

impl TaskStatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    Search {
        #[command(flatten)]
        path: PathArg,
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, value_enum)]
        status: Option<TaskStatusArg>,
        #[arg(long, default_value_t = 0)]
        after_sequence: u64,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=50))]
        limit: u16,
    },
    Create(TaskCreateArgs),
    Update(TaskUpdateArgs),
    History {
        task_id: String,
    },
}

#[derive(Debug, Args)]
struct TaskCreateArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    title: String,
    #[arg(long, value_enum)]
    kind: TaskKindArg,
    #[arg(long)]
    outcome: Option<String>,
    #[arg(long)]
    impact: Option<String>,
    #[arg(long)]
    unblock_condition: Option<String>,
    #[arg(long)]
    verification: Option<String>,
    #[arg(long)]
    technical_note: Option<String>,
    #[arg(long)]
    parent_task_id: Option<String>,
    #[arg(long)]
    release_id: Option<String>,
    #[arg(long)]
    estimated_loc: Option<u32>,
}

#[derive(Debug, Args)]
struct TaskUpdateArgs {
    task_id: String,
    #[arg(long, value_enum)]
    status: Option<TaskStatusArg>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    outcome: Option<String>,
    #[arg(long)]
    impact: Option<String>,
    #[arg(long)]
    unblock_condition: Option<String>,
    #[arg(long)]
    verification: Option<String>,
    #[arg(long)]
    technical_note: Option<String>,
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    parent_task_id: Option<String>,
    #[arg(long)]
    release_id: Option<String>,
    #[arg(long)]
    estimated_loc: Option<u32>,
    #[arg(long)]
    position: Option<u32>,
    #[arg(long)]
    backlog: bool,
    #[arg(long)]
    root: bool,
    #[arg(long, conflicts_with = "elaboration_complete")]
    elaboration_needed: bool,
    #[arg(long, conflicts_with = "elaboration_needed")]
    elaboration_complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum ReleaseKindArg {
    Preview,
    Release,
}

impl ReleaseKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Release => "release",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum ReleaseUpdateStatusArg {
    Planned,
    Dropped,
}

impl ReleaseUpdateStatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug, Subcommand)]
enum ReleaseCommand {
    Create(ReleaseCreateArgs),
    Update(ReleaseUpdateArgs),
    Request(ReleaseRequestArgs),
    Deliver(ReleaseDeliverArgs),
    DeliverEvidence {
        #[arg(long)]
        file: PathBuf,
    },
    Evidence {
        reference: String,
    },
    EvidenceShow {
        release_id: String,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 10)]
        limit: u8,
    },
}

#[derive(Debug, Args)]
struct ReleaseCreateArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    name: String,
    #[arg(long, value_enum)]
    kind: ReleaseKindArg,
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    seq: Option<u32>,
}

#[derive(Debug, Args)]
struct ReleaseUpdateArgs {
    #[arg(long)]
    release_id: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    seq: Option<u32>,
    #[arg(long)]
    note: Option<String>,
    #[arg(long, value_enum)]
    status: Option<ReleaseUpdateStatusArg>,
}

#[derive(Debug, Args)]
struct ReleaseRequestArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    note: Option<String>,
}

#[derive(Debug, Args)]
struct ReleaseDeliverArgs {
    #[arg(long)]
    release_id: String,
    #[arg(long)]
    deployment_id: String,
    #[arg(long)]
    note: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum DecisionAspectArg {
    Ui,
    Architecture,
    Algorithms,
    BusinessLogic,
    Data,
    Testing,
    Deployment,
    Security,
    Performance,
    Process,
    Other,
}

impl DecisionAspectArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Architecture => "architecture",
            Self::Algorithms => "algorithms",
            Self::BusinessLogic => "business_logic",
            Self::Data => "data",
            Self::Testing => "testing",
            Self::Deployment => "deployment",
            Self::Security => "security",
            Self::Performance => "performance",
            Self::Process => "process",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Subcommand)]
enum DecisionCommand {
    Record(DecisionRecordArgs),
    Tail(DecisionTailArgs),
    Search(DecisionSearchArgs),
    Summarize(DecisionSummarizeArgs),
}

#[derive(Debug, Args)]
struct DecisionRecordArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long, value_enum)]
    aspect: DecisionAspectArg,
    #[arg(long)]
    title: String,
    #[arg(long)]
    body: String,
    #[arg(long)]
    technical_note: Option<String>,
    #[arg(long = "ref")]
    reference: Option<String>,
    #[arg(long)]
    supersedes: Option<String>,
}

#[derive(Debug, Args)]
struct DecisionTailArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long, value_enum)]
    aspect: Option<DecisionAspectArg>,
    #[arg(short = 'n')]
    n: Option<u8>,
}

#[derive(Debug, Args)]
struct DecisionSearchArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    query: String,
    #[arg(long, value_enum)]
    aspect: Option<DecisionAspectArg>,
    #[arg(short = 'n')]
    n: Option<u8>,
}

#[derive(Debug, Args)]
struct DecisionSummarizeArgs {
    #[command(flatten)]
    path: PathArg,
    #[arg(long)]
    body: String,
    #[arg(long)]
    covers_through_seq: u32,
}

impl TestCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Start(args) => {
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                insert_option(&mut params, "test", args.test_name);
                if !args.checks.is_empty() {
                    params.insert(
                        "checks".to_owned(),
                        Value::Array(args.checks.into_iter().map(Value::String).collect()),
                    );
                }
                params.insert(
                    "tier".to_owned(),
                    Value::String(args.tier.as_str().to_owned()),
                );
                remote("test.start", Value::Object(params))
            }
            Self::Retry(args) => {
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert("run_id".to_owned(), Value::String(args.run_id));
                params.insert("check".to_owned(), Value::String(args.check));
                insert_option(&mut params, "test", args.test_name);
                remote("test.retry", Value::Object(params))
            }
            Self::Status(path) => remote("test.status", path_params(&path)?),
            Self::History {
                path,
                before,
                limit,
            } => {
                let mut params = object(path_params(&path)?);
                insert_option(&mut params, "before", before);
                params.insert("limit".to_owned(), json!(limit));
                remote("test.history", Value::Object(params))
            }
            Self::Log { command } => command.into_invocation(),
            Self::Evidence { command } => command.into_invocation(),
            Self::Artifact { command } => command.into_invocation(),
            Self::Stop(args) => {
                let mut params = object(path_params(&args.path)?);
                insert_option(&mut params, "reason", args.reason);
                remote("test.stop", Value::Object(params))
            }
            Self::Event { status } => Ok(Invocation::TestEvent { status }),
            Self::List {
                after_worktree_id,
                limit,
            } => remote(
                "test.list",
                json!({"after_worktree_id": after_worktree_id, "limit": limit}),
            ),
            Self::Capacity { command } => match command {
                CapacityCommand::Show => remote("test.capacity.get", json!({})),
                CapacityCommand::Set { cap } => {
                    if cap == 0 {
                        return Err(invalid("test capacity must be at least 1"));
                    }
                    remote("test.capacity.set", json!({"cap": cap}))
                }
                CapacityCommand::Clear => remote("test.capacity.set", json!({"cap": null})),
            },
        }
    }
}

impl OptionalLogSelector {
    fn into_params(self) -> Result<Map<String, Value>, CliValidationError> {
        let mut params = Map::new();
        params.insert("path".to_owned(), Value::String(self.path.absolute()?));
        insert_option(&mut params, "run_id", self.run_id);
        insert_option(&mut params, "check", self.check);
        if let Some(phase) = self.phase {
            params.insert("phase".to_owned(), Value::String(phase.as_str().to_owned()));
        }
        insert_option(&mut params, "case", self.case_name);
        if let Some(stream) = self.stream {
            params.insert(
                "stream".to_owned(),
                Value::String(stream.as_str().to_owned()),
            );
        }
        insert_option(&mut params, "cursor", self.cursor);
        Ok(params)
    }
}

impl RequiredLogSelector {
    fn into_params(self) -> Result<Map<String, Value>, CliValidationError> {
        let mut params = Map::new();
        params.insert("path".to_owned(), Value::String(self.path.absolute()?));
        insert_option(&mut params, "run_id", self.run_id);
        insert_option(&mut params, "check", self.check);
        params.insert(
            "phase".to_owned(),
            Value::String(self.phase.as_str().to_owned()),
        );
        insert_option(&mut params, "case", self.case_name);
        params.insert(
            "stream".to_owned(),
            Value::String(self.stream.as_str().to_owned()),
        );
        insert_option(&mut params, "cursor", self.cursor);
        Ok(params)
    }
}

impl TestLogCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Catalog { selector, limit } => {
                require_range("--limit", limit, 1, 100)?;
                let mut params = selector.into_params()?;
                params.insert("limit".to_owned(), json!(limit));
                remote("test.log.catalog", Value::Object(params))
            }
            Self::Tail {
                selector,
                lines,
                max_bytes,
            } => {
                require_range("--lines", lines, 1, 5_000)?;
                require_range("--max-bytes", max_bytes, 1, 49_152)?;
                let mut params = selector.into_params()?;
                params.insert("lines".to_owned(), json!(lines));
                params.insert("max_bytes".to_owned(), json!(max_bytes));
                remote("test.log.tail", Value::Object(params))
            }
            Self::Search {
                selector,
                text,
                max_matches,
                context_lines,
                max_bytes,
            } => {
                require_range("--max-matches", max_matches, 1, 100)?;
                require_range("--context-lines", context_lines, 0, 100)?;
                require_range("--max-bytes", max_bytes, 1, 49_152)?;
                let mut params = selector.into_params()?;
                params.insert("text".to_owned(), Value::String(text));
                params.insert("max_matches".to_owned(), json!(max_matches));
                params.insert("context_lines".to_owned(), json!(context_lines));
                params.insert("max_bytes".to_owned(), json!(max_bytes));
                remote("test.log.search", Value::Object(params))
            }
            Self::Range {
                selector,
                line_start,
                line_end,
                byte_start,
                byte_end,
                max_bytes,
            } => {
                require_range("--max-bytes", max_bytes, 1, 49_152)?;
                match (line_start, line_end, byte_start, byte_end) {
                    (Some(start), Some(end), None, None) if start <= end => {
                        let mut params = selector.into_params()?;
                        params.insert("line_start".to_owned(), json!(start));
                        params.insert("line_end".to_owned(), json!(end));
                        params.insert("max_bytes".to_owned(), json!(max_bytes));
                        remote("test.log.range", Value::Object(params))
                    }
                    (None, None, Some(start), Some(end)) if start <= end => {
                        let mut params = selector.into_params()?;
                        params.insert("byte_start".to_owned(), json!(start));
                        params.insert("byte_end".to_owned(), json!(end));
                        params.insert("max_bytes".to_owned(), json!(max_bytes));
                        remote("test.log.range", Value::Object(params))
                    }
                    _ => Err(invalid(
                        "log range requires exactly one complete ordered line or byte interval",
                    )),
                }
            }
            Self::FailureContext {
                selector,
                limit,
                context_lines,
                max_bytes,
            } => {
                require_range("--limit", limit, 1, 100)?;
                require_range("--context-lines", context_lines, 0, 100)?;
                require_range("--max-bytes", max_bytes, 1, 49_152)?;
                let mut params = selector.into_params()?;
                params.insert("limit".to_owned(), json!(limit));
                params.insert("context_lines".to_owned(), json!(context_lines));
                params.insert("max_bytes".to_owned(), json!(max_bytes));
                remote("test.log.failure_context", Value::Object(params))
            }
            Self::Retention { command } => match command {
                RetentionCommand::Show => remote("test.log.retention.get", json!({})),
                RetentionCommand::Set {
                    max_age_seconds,
                    case_depth,
                } => {
                    require_range("--max-age-seconds", max_age_seconds, 1, 315_360_000)?;
                    if case_depth == 0 {
                        return Err(invalid("--case-depth must be at least 1"));
                    }
                    remote(
                        "test.log.retention.set",
                        json!({
                            "max_age_seconds":max_age_seconds,
                            "case_depth":case_depth
                        }),
                    )
                }
            },
        }
    }
}

impl EvidenceCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Lookup {
                run_id,
                image_id,
                worktree_id,
            } => remote(
                "test.evidence.lookup",
                json!({"run_id":run_id,"image_id":image_id,"worktree_id":worktree_id}),
            ),
            Self::Show(args) => remote(
                "test.evidence.get",
                json!({"path":args.path.absolute()?,"run_id":args.run_id}),
            ),
            Self::Image(args) => {
                require_range("--max-bytes", args.max_bytes, 1, 184_320)?;
                remote(
                    "test.evidence.image",
                    json!({
                        "path":args.path.absolute()?,
                        "run_id":args.run_id,
                        "image_id":args.image_id,
                        "offset":args.offset,
                        "max_bytes":args.max_bytes
                    }),
                )
            }
            Self::Feedback { command } => command.into_invocation(),
        }
    }
}

impl FeedbackCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Create(args) => {
                let marks = serde_json::from_str::<Value>(&args.marks_json).map_err(|error| {
                    invalid(format!("--marks-json must be valid JSON: {error}"))
                })?;
                remote(
                    "test.evidence.feedback.create",
                    json!({
                        "path":args.path.absolute()?,
                        "run_id":args.run_id,
                        "image_id":args.image_id,
                        "body":args.body,
                        "marks":marks
                    }),
                )
            }
            Self::Reply(args) => remote(
                "test.evidence.feedback.reply",
                json!({
                    "path":args.path.absolute()?,
                    "run_id":args.run_id,
                    "feedback_id":args.feedback_id,
                    "body":args.body
                }),
            ),
            Self::Edit(args) => remote(
                "test.evidence.feedback.edit",
                json!({
                    "path":args.path.absolute()?,
                    "run_id":args.run_id,
                    "feedback_id":args.feedback_id,
                    "comment_id":args.comment_id,
                    "body":args.body
                }),
            ),
            Self::State(args) => remote("test.evidence.feedback.state", {
                let (path, state) = feedback_state_positionals(args.path_and_state)?;
                json!({
                    "path":path.absolute()?,
                    "run_id":args.run_id,
                    "feedback_id":args.feedback_id,
                    "state":state.as_str()
                })
            }),
            Self::Delete(args) => remote(
                "test.evidence.feedback.delete",
                json!({
                    "path":args.path.absolute()?,
                    "run_id":args.run_id,
                    "feedback_id":args.feedback_id
                }),
            ),
        }
    }
}

impl ArtifactCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Catalog(args) => {
                require_range("--limit", args.limit, 1, 100)?;
                if args.manifest_sha256.is_some() && args.artifact.is_none() {
                    return Err(invalid("--manifest-sha256 requires an exact --artifact"));
                }
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert("run_id".to_owned(), Value::String(args.run_id));
                params.insert("check".to_owned(), Value::String(args.check));
                insert_option(&mut params, "artifact", args.artifact);
                insert_option(&mut params, "manifest_sha256", args.manifest_sha256);
                params.insert("offset".to_owned(), json!(args.offset));
                params.insert("limit".to_owned(), json!(args.limit));
                remote("test.artifact.catalog", Value::Object(params))
            }
            Self::File(args) => {
                require_range("--max-bytes", args.max_bytes, 1, 184_320)?;
                remote(
                    "test.artifact.file",
                    json!({
                        "path":args.path.absolute()?,
                        "run_id":args.run_id,
                        "check":args.check,
                        "artifact":args.artifact,
                        "file":args.file_name,
                        "manifest_sha256":args.manifest_sha256,
                        "offset":args.offset,
                        "max_bytes":args.max_bytes
                    }),
                )
            }
            Self::Materialize(args) => {
                if !args.destination.is_absolute() {
                    return Err(invalid("--destination must be absolute"));
                }
                let mut unique = HashSet::new();
                if args
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.is_empty() || !unique.insert(artifact))
                {
                    return Err(invalid(
                        "requested retained artifact names must be non-empty and unique",
                    ));
                }
                Ok(Invocation::ArtifactMaterialize {
                    path: args.path.absolute()?,
                    run_id: args.run_id,
                    check: args.check,
                    artifacts: args.artifacts,
                    destination: args.destination,
                })
            }
        }
    }
}

impl DeploymentCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::List { path } => {
                if let Some(path) = path {
                    let path = absolute_path(path)?;
                    remote("deployment.list", json!({"path":path}))
                } else {
                    remote("deployment.list", json!({}))
                }
            }
            Self::Apply(args) => remote("deployment.apply", Value::Object(args.selector.params()?)),
            Self::Preflight(args) => remote(
                "deployment.preflight",
                Value::Object(args.selector.params()?),
            ),
            Self::Status(args) => {
                remote("deployment.status", Value::Object(args.selector.params()?))
            }
            Self::Rollback(args) => remote(
                "deployment.rollback",
                Value::Object(args.selector.params()?),
            ),
            Self::Start(args) => deployment_control("deployment.start", args),
            Self::Stop(args) => deployment_control("deployment.stop", args),
            Self::Restart(args) => deployment_control("deployment.restart", args),
            Self::Logs(args) => {
                require_range("--tail-lines", args.tail_lines, 1, 5_000)?;
                let component = args
                    .component
                    .ok_or_else(|| invalid("deployment logs requires --component"))?;
                let mut params = args.selector.params()?;
                params.insert("component".to_owned(), Value::String(component));
                params.insert("tail_lines".to_owned(), json!(args.tail_lines));
                remote("deployment.logs", Value::Object(params))
            }
            Self::Remove(args) => {
                let mut params = args.selector.params()?;
                params.insert("delete_data".to_owned(), Value::Bool(args.delete_data));
                remote("deployment.remove", Value::Object(params))
            }
            Self::SetDomain(args) => {
                let domain_selected = args.domain.as_ref().is_some_and(|value| !value.is_empty());
                if domain_selected == args.clear {
                    return Err(invalid("pass exactly one of --domain <label> or --clear"));
                }
                let mut params = Map::new();
                params.insert(
                    "deployment_id".to_owned(),
                    Value::String(args.deployment_id),
                );
                params.insert(
                    "domain".to_owned(),
                    args.domain.map_or(Value::Null, Value::String),
                );
                if let Some(port) = args.port {
                    params.insert("port".to_owned(), json!(port));
                }
                insert_option(&mut params, "component", args.component);
                if args.public {
                    params.insert("public".to_owned(), Value::Bool(true));
                } else if args.authenticated {
                    params.insert("public".to_owned(), Value::Bool(false));
                }
                remote("deployment.set_domain", Value::Object(params))
            }
        }
    }
}

fn deployment_control(
    operation_name: &'static str,
    args: DeploymentControlArgs,
) -> Result<Invocation, CliValidationError> {
    let mut params = args.selector.params()?;
    insert_option(&mut params, "component", args.component);
    remote(operation_name, Value::Object(params))
}

fn feedback_state_positionals(
    values: Vec<String>,
) -> Result<(PathArg, FeedbackStateArg), CliValidationError> {
    let (path, state) = match values.as_slice() {
        [state] => (None, state.as_str()),
        [path, state] => (Some(PathBuf::from(path)), state.as_str()),
        _ => return Err(invalid("feedback state requires STATE and optional PATH")),
    };
    let state = match state {
        "open" => FeedbackStateArg::Open,
        "resolved" => FeedbackStateArg::Resolved,
        _ => return Err(invalid("feedback state must be open or resolved")),
    };
    Ok((PathArg { path }, state))
}

impl HealthCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Containers => remote("health.containers", json!({})),
            Self::Summary => remote("health.summary", json!({})),
            Self::Repositories => remote("health.repositories", json!({})),
            Self::Repository(path) => remote("health.repository", path_params(&path)?),
            Self::History {
                subject_kind,
                subject_id,
                metric,
                minutes,
            } => {
                require_range("--minutes", minutes, 1, 43_200)?;
                remote(
                    "health.history",
                    json!({
                        "subject_kind":subject_kind.as_str(),
                        "subject_id":subject_id,
                        "metric":metric.as_str(),
                        "minutes":minutes
                    }),
                )
            }
        }
    }
}

impl TelegramCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::List => remote("telegram.list", json!({})),
            Self::Link { code, email } => {
                remote("telegram.link", json!({"code":code,"email":email}))
            }
            Self::Subscribe { chat_id, scope } => remote(
                "telegram.subscribe",
                json!({"chat_id":chat_id,"scope":scope}),
            ),
            Self::Unsubscribe { chat_id, scope } => remote(
                "telegram.unsubscribe",
                json!({"chat_id":chat_id,"scope":scope}),
            ),
        }
    }
}

impl EventCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Wait {
                cursor,
                filters,
                limit,
            } => {
                require_range("--limit", u32::from(limit), 1, 100)?;
                let filters = filters
                    .into_iter()
                    .map(|filter| {
                        serde_json::from_str::<devcoordinator2_api::params::EventFilter>(&filter)
                            .map_err(|error| invalid(format!("--filter is invalid JSON: {error}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                remote(
                    "event.wait",
                    serde_json::to_value(devcoordinator2_api::params::EventWait {
                        cursor,
                        filters,
                        limit,
                    })
                    .map_err(|error| {
                        invalid(format!("event wait parameters are invalid: {error}"))
                    })?,
                )
            }
        }
    }
}

impl RepositoryCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::List { include_archived } => {
                let params = if include_archived {
                    json!({"include_archived":true})
                } else {
                    json!({})
                };
                remote("repository.list", params)
            }
            Self::Status(path) => remote("repository.status", path_params(&path)?),
            Self::Register(path) => remote("repository.register", path_params(&path)?),
            Self::Archive {
                repository_id,
                merged_into_repository_id,
                note,
            } => remote(
                "repository.archive",
                json!({
                    "repository_id":repository_id,
                    "merged_into_repository_id":merged_into_repository_id,
                    "note":note
                }),
            ),
            Self::Unarchive {
                repository_id,
                note,
            } => remote(
                "repository.unarchive",
                json!({"repository_id":repository_id,"note":note}),
            ),
        }
    }
}

impl PlanCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Overview {
                path,
                all_repositories,
            } => {
                if all_repositories {
                    remote("plan.overview", json!({}))
                } else {
                    remote("plan.overview", path_params(&path)?)
                }
            }
        }
    }
}

impl TaskCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Create(args) => {
                if args.estimated_loc == Some(0) {
                    return Err(invalid("--estimated-loc must be at least 1"));
                }
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert("title".to_owned(), Value::String(args.title));
                params.insert(
                    "kind".to_owned(),
                    Value::String(args.kind.as_str().to_owned()),
                );
                insert_option(&mut params, "outcome", args.outcome);
                insert_option(&mut params, "impact", args.impact);
                insert_option(&mut params, "unblock_condition", args.unblock_condition);
                insert_option(&mut params, "verification", args.verification);
                insert_option(&mut params, "technical_note", args.technical_note);
                insert_option(&mut params, "parent_task_id", args.parent_task_id);
                insert_option(&mut params, "release_id", args.release_id);
                insert_number_option(&mut params, "estimated_loc", args.estimated_loc);
                remote("task.create", Value::Object(params))
            }
            Self::Update(args) => {
                if args.backlog && args.release_id.is_some() {
                    return Err(invalid("pass --release-id or --backlog, not both"));
                }
                if args.root && args.parent_task_id.is_some() {
                    return Err(invalid("pass --parent-task-id or --root, not both"));
                }
                let mut params = Map::new();
                params.insert("task_id".to_owned(), Value::String(args.task_id));
                insert_option(&mut params, "title", args.title);
                insert_option(&mut params, "outcome", args.outcome);
                insert_option(&mut params, "impact", args.impact);
                insert_option(&mut params, "unblock_condition", args.unblock_condition);
                insert_option(&mut params, "verification", args.verification);
                insert_option(&mut params, "technical_note", args.technical_note);
                insert_option(&mut params, "note", args.note);
                if let Some(status) = args.status {
                    params.insert(
                        "status".to_owned(),
                        Value::String(status.as_str().to_owned()),
                    );
                }
                insert_number_option(&mut params, "estimated_loc", args.estimated_loc);
                insert_number_option(&mut params, "position", args.position);
                if args.backlog {
                    params.insert("release_id".to_owned(), Value::Null);
                } else {
                    insert_option(&mut params, "release_id", args.release_id);
                }
                if args.root {
                    params.insert("parent_task_id".to_owned(), Value::Null);
                } else {
                    insert_option(&mut params, "parent_task_id", args.parent_task_id);
                }
                if args.elaboration_needed {
                    params.insert("elaboration_needed".to_owned(), Value::Bool(true));
                } else if args.elaboration_complete {
                    params.insert("elaboration_needed".to_owned(), Value::Bool(false));
                }
                remote("task.update", Value::Object(params))
            }
            Self::Search {
                path,
                query,
                status,
                after_sequence,
                limit,
            } => remote(
                "task.search",
                json!({
                    "path": path.absolute()?, "query": query, "status": status.map(TaskStatusArg::as_str), "after_sequence": after_sequence, "limit": limit
                }),
            ),
            Self::History { task_id } => remote("task.history", json!({"task_id":task_id})),
        }
    }
}

impl ReleaseCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::DeliverEvidence { file } => {
                remote("release.deliver_evidence", review_cli::bounded_file(&file)?)
            }
            Self::Evidence { reference } => {
                remote("release.evidence", json!({"reference":reference}))
            }
            Self::EvidenceShow {
                release_id,
                offset,
                limit,
            } => remote(
                "release.evidence_show",
                json!({"release_id":release_id,"offset":offset,"limit":limit}),
            ),
            Self::Create(args) => {
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert("name".to_owned(), Value::String(args.name));
                params.insert(
                    "kind".to_owned(),
                    Value::String(args.kind.as_str().to_owned()),
                );
                insert_option(&mut params, "note", args.note);
                insert_number_option(&mut params, "seq", args.seq);
                remote("release.create", Value::Object(params))
            }
            Self::Update(args) => {
                let mut params = Map::new();
                params.insert("release_id".to_owned(), Value::String(args.release_id));
                insert_option(&mut params, "name", args.name);
                insert_number_option(&mut params, "seq", args.seq);
                insert_option(&mut params, "note", args.note);
                if let Some(status) = args.status {
                    params.insert(
                        "status".to_owned(),
                        Value::String(status.as_str().to_owned()),
                    );
                }
                remote("release.update", Value::Object(params))
            }
            Self::Request(args) => {
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                insert_option(&mut params, "name", args.name);
                insert_option(&mut params, "note", args.note);
                remote("release.request", Value::Object(params))
            }
            Self::Deliver(args) => {
                let mut params = Map::new();
                params.insert("release_id".to_owned(), Value::String(args.release_id));
                params.insert(
                    "deployment_id".to_owned(),
                    Value::String(args.deployment_id),
                );
                insert_option(&mut params, "note", args.note);
                remote("release.deliver", Value::Object(params))
            }
        }
    }
}

impl DecisionCommand {
    fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Record(args) => {
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert(
                    "aspect".to_owned(),
                    Value::String(args.aspect.as_str().to_owned()),
                );
                params.insert("title".to_owned(), Value::String(args.title));
                params.insert("body".to_owned(), Value::String(args.body));
                insert_option(&mut params, "technical_note", args.technical_note);
                insert_option(&mut params, "ref", args.reference);
                insert_option(&mut params, "supersedes", args.supersedes);
                remote("decision.record", Value::Object(params))
            }
            Self::Tail(args) => {
                validate_decision_limit(args.n)?;
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                if let Some(aspect) = args.aspect {
                    params.insert(
                        "aspect".to_owned(),
                        Value::String(aspect.as_str().to_owned()),
                    );
                }
                insert_number_option(&mut params, "n", args.n);
                remote("decision.tail", Value::Object(params))
            }
            Self::Search(args) => {
                validate_decision_limit(args.n)?;
                let mut params = Map::new();
                params.insert("path".to_owned(), Value::String(args.path.absolute()?));
                params.insert("query".to_owned(), Value::String(args.query));
                if let Some(aspect) = args.aspect {
                    params.insert(
                        "aspect".to_owned(),
                        Value::String(aspect.as_str().to_owned()),
                    );
                }
                insert_number_option(&mut params, "n", args.n);
                remote("decision.search", Value::Object(params))
            }
            Self::Summarize(args) => remote(
                "decision.summarize",
                json!({
                    "path":args.path.absolute()?,
                    "body":args.body,
                    "covers_through_seq":args.covers_through_seq
                }),
            ),
        }
    }
}

fn validate_decision_limit(limit: Option<u8>) -> Result<(), CliValidationError> {
    if let Some(limit) = limit {
        require_range("-n", limit, 1, 50)?;
    }
    Ok(())
}

fn path_params(path: &PathArg) -> Result<Value, CliValidationError> {
    Ok(json!({"path":path.absolute()?}))
}

fn absolute_path(path: PathBuf) -> Result<String, CliValidationError> {
    std::path::absolute(path)
        .map_err(CliValidationError::Path)?
        .into_os_string()
        .into_string()
        .map_err(|_| invalid("path must be valid UTF-8"))
}

fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("path_params always returns an object")
        .clone()
}

fn insert_option(params: &mut Map<String, Value>, name: &str, value: Option<String>) {
    if let Some(value) = value {
        params.insert(name.to_owned(), Value::String(value));
    }
}

fn insert_number_option<T>(params: &mut Map<String, Value>, name: &str, value: Option<T>)
where
    Value: From<T>,
{
    if let Some(value) = value {
        params.insert(name.to_owned(), Value::from(value));
    }
}

fn require_range<T>(name: &str, value: T, minimum: T, maximum: T) -> Result<(), CliValidationError>
where
    T: Copy + Ord + std::fmt::Display,
{
    if value < minimum || value > maximum {
        Err(invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )))
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> CliValidationError {
    CliValidationError::Invalid(message.into())
}

fn remote(operation_name: &'static str, params: Value) -> Result<Invocation, CliValidationError> {
    let definition =
        operation(operation_name).ok_or(CliValidationError::MissingOperation(operation_name))?;
    if !definition
        .cli_routes
        .iter()
        .any(|route| route.dispatch == CliDispatch::Protocol)
    {
        return Err(CliValidationError::MissingCliRoute(operation_name));
    }
    (definition.validate_params)(&params).map_err(|source| CliValidationError::Contract {
        operation: operation_name,
        source,
    })?;
    Ok(Invocation::Remote {
        operation: operation_name,
        params,
    })
}

/// Render one protocol-v2 response to stdout. JSON is the stable automation
/// surface; human output keeps every returned value and every error field.
pub fn render_response(
    response: &ResponseEnvelope,
    format: OutputFormat,
    operation: Option<&str>,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    render_response_to(&mut output, response, format, operation)
}

fn render_response_to(
    output: &mut impl Write,
    response: &ResponseEnvelope,
    format: OutputFormat,
    operation: Option<&str>,
) -> io::Result<()> {
    match format {
        OutputFormat::Json => {
            serde_json::to_writer_pretty(&mut *output, response).map_err(io::Error::other)?;
            writeln!(output)
        }
        OutputFormat::Human => match response {
            ResponseEnvelope::Success { data, .. } => render_human_success(output, data, operation),
            ResponseEnvelope::Failure { error, .. } => {
                writeln!(output, "error {}: {}", error.code, error.message)?;
                if !error.detail.is_empty() {
                    writeln!(output, "detail: {}", error.detail)?;
                }
                Ok(())
            }
        },
    }
}

fn render_human_success(
    output: &mut impl Write,
    data: &Value,
    operation: Option<&str>,
) -> io::Result<()> {
    if operation == Some("ping")
        && let Some(object) = data.as_object()
    {
        let daemon_version = object.get("daemon_version").and_then(Value::as_str);
        let protocol = object.get("protocol_version").and_then(Value::as_u64);
        let schema = object.get("schema_version").and_then(Value::as_u64);
        if let (Some(daemon_version), Some(protocol), Some(schema)) =
            (daemon_version, protocol, schema)
        {
            writeln!(
                output,
                "DevCoordinator2 {daemon_version} — protocol {protocol}, schema {schema}"
            )?;
            let mut remainder = object.clone();
            remainder.remove("daemon_version");
            remainder.remove("protocol_version");
            remainder.remove("schema_version");
            if !remainder.is_empty() {
                write_pretty_value(output, &Value::Object(remainder))?;
            }
            return Ok(());
        }
    }
    let label = operation.unwrap_or("success");
    if data.as_object().is_some_and(Map::is_empty) {
        return writeln!(output, "{label}: ok");
    }
    writeln!(output, "{label}:")?;
    write_pretty_value(output, data)
}

fn write_pretty_value(output: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *output, value).map_err(io::Error::other)?;
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use devcoordinator2_api::ErrorCode;

    fn invocation(args: &[&str]) -> Result<Invocation, CliValidationError> {
        let argv = std::iter::once("devcoordinator2")
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        Cli::try_parse_from(argv)
            .unwrap_or_else(|error| panic!("failed to parse {args:?}: {error}"))
            .into_invocation()
    }

    fn remote_invocation(args: &[&str]) -> (&'static str, Value) {
        match invocation(args).unwrap_or_else(|error| panic!("{args:?}: {error}")) {
            Invocation::Remote { operation, params } => {
                assert!(
                    devcoordinator2_api::operation(operation).is_some(),
                    "{args:?} mapped to an unregistered operation {operation}"
                );
                (operation, params)
            }
            other => panic!("{args:?} unexpectedly mapped to {other:?}"),
        }
    }

    #[test]
    fn clap_hierarchy_and_every_remote_route_map_to_protocol_two_registry() {
        Cli::command().debug_assert();
        let glossary_inputs = tempfile::tempdir().unwrap();
        let concept_file = glossary_inputs.path().join("concept.json");
        let settings_file = glossary_inputs.path().join("settings.json");
        let usages_file = glossary_inputs.path().join("usages.json");
        std::fs::write(
            &concept_file,
            r#"{"name":"Execution","definition":"One test execution"}"#,
        )
        .unwrap();
        std::fs::write(&settings_file, r#"{"languages":["en"],"guidelines":[]}"#).unwrap();
        std::fs::write(&usages_file, "[]").unwrap();
        let review_file = glossary_inputs.path().join("review.json");
        std::fs::write(&review_file, serde_json::to_vec(&json!({
            "version":1,"repositoryId":"project-alpha","projectId":"project-alpha","windowStartMs":1000000,"windowEndMs":605800000,
            "experiment":{"hypothesis":"Retain the required checks","evidenceRefs":[],"alternatives":["Keep checks","Remove checks"],"chosenAction":"Keep required checks",
            "baseline":{"evidenceRefs":[],"interpretation":"No per-task measurements","missingMeasurements":[]},"successCriteria":"Preserve required quality","rollbackCondition":"Reconsider with evidence",
            "disposition":"proposed","resultEvidenceRefs":[],"scopeRepoId":"project-alpha","preservesQuality":true,"reason":"Retain all required checks","observations":[]}
        })).unwrap()).unwrap();
        let delivery_file = glossary_inputs.path().join("delivery.json");
        std::fs::write(&delivery_file, serde_json::to_vec(&json!({"release_id":"release-alpha","path":"/tmp/repo","run_id":"run","check":"build","artifact":"package","manifest_sha256":"a".repeat(64),"source_sha256":"b".repeat(64),"target":"linux-cli","kind":"local-executable"})).unwrap()).unwrap();
        let cases: &[(&[&str], &str)] = &[
            (
                &[
                    "review",
                    "prepare",
                    "--repository-id",
                    "project-alpha",
                    "--window-start-ms",
                    "1000000",
                    "--window-end-ms",
                    "605800000",
                ],
                "review.prepare",
            ),
            (
                &[
                    "review",
                    "record",
                    "--file",
                    review_file.to_str().unwrap(),
                    "--expected-revision",
                    "0",
                ],
                "review.record",
            ),
            (
                &["review", "list", "--repository-id", "project-alpha"],
                "review.show",
            ),
            (&["review", "show", "review-fixture@1"], "review.receipt"),
            (
                &[
                    "release",
                    "deliver-evidence",
                    "--file",
                    delivery_file.to_str().unwrap(),
                ],
                "release.deliver_evidence",
            ),
            (
                &["release", "evidence-show", "release-alpha"],
                "release.evidence_show",
            ),
            (
                &["release", "evidence", "delivery-fixture"],
                "release.evidence",
            ),
            (&["ping"], "ping"),
            (&["config", "show"], "config.get"),
            (
                &[
                    "config",
                    "authorize",
                    "/tmp/repo",
                    "--name",
                    "web",
                    "--file",
                    ".env",
                    "--expected-revision",
                    "revision",
                ],
                "config.env.set",
            ),
            (
                &[
                    "config",
                    "revoke",
                    "--deployment-id",
                    "d1",
                    "--file",
                    ".env",
                    "--expected-revision",
                    "revision",
                ],
                "config.env.set",
            ),
            (
                &["config", "reload", "--expected-revision", "revision"],
                "config.reload",
            ),
            (
                &["deployment", "preflight", "/tmp/repo", "--name", "web"],
                "deployment.preflight",
            ),
            (&["glossary", "list"], "glossary.list"),
            (&["glossary", "resolve"], "glossary.resolve"),
            (&["glossary", "get", "g1111111111111111"], "glossary.get"),
            (
                &[
                    "glossary",
                    "save",
                    "--expected-revision",
                    "0",
                    "--file",
                    concept_file.to_str().unwrap(),
                ],
                "glossary.save",
            ),
            (
                &[
                    "glossary",
                    "configure",
                    "--expected-revision",
                    "0",
                    "--file",
                    settings_file.to_str().unwrap(),
                ],
                "glossary.configure",
            ),
            (
                &[
                    "glossary",
                    "inherit",
                    "g1111111111111111",
                    "--expected-revision",
                    "1",
                ],
                "glossary.inherit",
            ),
            (&["glossary", "history"], "glossary.history"),
            (
                &["glossary", "check", "--file", usages_file.to_str().unwrap()],
                "glossary.check",
            ),
            (&["glossary", "impact"], "glossary.impact"),
            (
                &["test", "start", "/tmp/repo", "--check", "unit"],
                "test.start",
            ),
            (
                &[
                    "test",
                    "retry",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--check",
                    "unit",
                ],
                "test.retry",
            ),
            (&["test", "status", "/tmp/repo"], "test.status"),
            (
                &[
                    "test",
                    "history",
                    "/tmp/repo",
                    "--limit",
                    "5",
                    "--before",
                    "t20260101T000000Z-abc123",
                ],
                "test.history",
            ),
            (&["test", "log", "catalog", "/tmp/repo"], "test.log.catalog"),
            (
                &[
                    "test",
                    "log",
                    "tail",
                    "/tmp/repo",
                    "--phase",
                    "check",
                    "--stream",
                    "stderr",
                ],
                "test.log.tail",
            ),
            (
                &[
                    "test",
                    "log",
                    "search",
                    "/tmp/repo",
                    "--phase",
                    "check",
                    "--stream",
                    "stdout",
                    "--text",
                    "literal.*text",
                ],
                "test.log.search",
            ),
            (
                &[
                    "test",
                    "log",
                    "range",
                    "/tmp/repo",
                    "--phase",
                    "case",
                    "--stream",
                    "stdout",
                    "--line-start",
                    "1",
                    "--line-end",
                    "20",
                ],
                "test.log.range",
            ),
            (
                &[
                    "test",
                    "log",
                    "failure-context",
                    "/tmp/repo",
                    "--phase",
                    "check",
                    "--stream",
                    "stderr",
                ],
                "test.log.failure_context",
            ),
            (
                &["test", "log", "retention", "show"],
                "test.log.retention.get",
            ),
            (
                &[
                    "test",
                    "log",
                    "retention",
                    "set",
                    "--max-age-seconds",
                    "7200",
                    "--case-depth",
                    "5",
                ],
                "test.log.retention.set",
            ),
            (
                &["test", "evidence", "show", "/tmp/repo", "--run-id", "trun"],
                "test.evidence.get",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "image",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--image-id",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ],
                "test.evidence.image",
            ),
            (
                &["test", "evidence", "lookup", "--run-id", "trun"],
                "test.evidence.lookup",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "feedback",
                    "create",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--image-id",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "--body",
                    "Increase contrast",
                    "--marks-json",
                    r##"[{"type":"pin","id":"m1","color":"#fff","x":0.5,"y":0.5}]"##,
                ],
                "test.evidence.feedback.create",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "feedback",
                    "reply",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--feedback-id",
                    "f1",
                    "--body",
                    "Reply body",
                ],
                "test.evidence.feedback.reply",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "feedback",
                    "edit",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--feedback-id",
                    "f1",
                    "--comment-id",
                    "m1",
                    "--body",
                    "Edited body",
                ],
                "test.evidence.feedback.edit",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "feedback",
                    "state",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--feedback-id",
                    "f1",
                    "resolved",
                ],
                "test.evidence.feedback.state",
            ),
            (
                &[
                    "test",
                    "evidence",
                    "feedback",
                    "delete",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--feedback-id",
                    "f1",
                ],
                "test.evidence.feedback.delete",
            ),
            (
                &[
                    "test",
                    "artifact",
                    "catalog",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--check",
                    "browser",
                ],
                "test.artifact.catalog",
            ),
            (
                &[
                    "test",
                    "artifact",
                    "file",
                    "/tmp/repo",
                    "--run-id",
                    "trun",
                    "--check",
                    "browser",
                    "--artifact",
                    "production",
                    "--file",
                    "nested/report.json",
                    "--manifest-sha256",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ],
                "test.artifact.file",
            ),
            (&["test", "stop", "/tmp/repo"], "test.stop"),
            (&["test", "list"], "test.list"),
            (&["test", "capacity", "show"], "test.capacity.get"),
            (&["test", "capacity", "set", "8"], "test.capacity.set"),
            (&["test", "capacity", "clear"], "test.capacity.set"),
            (&["deployment", "list"], "deployment.list"),
            (
                &["deployment", "apply", "/tmp/repo", "--name", "web"],
                "deployment.apply",
            ),
            (
                &["deployment", "status", "/tmp/repo", "--deployment-id", "d1"],
                "deployment.status",
            ),
            (
                &[
                    "deployment",
                    "rollback",
                    "/tmp/repo",
                    "--deployment-id",
                    "d1",
                ],
                "deployment.rollback",
            ),
            (
                &["deployment", "start", "/tmp/repo", "--deployment-id", "d1"],
                "deployment.start",
            ),
            (
                &["deployment", "stop", "/tmp/repo", "--deployment-id", "d1"],
                "deployment.stop",
            ),
            (
                &[
                    "deployment",
                    "restart",
                    "/tmp/repo",
                    "--deployment-id",
                    "d1",
                ],
                "deployment.restart",
            ),
            (
                &[
                    "deployment",
                    "logs",
                    "/tmp/repo",
                    "--deployment-id",
                    "d1",
                    "--component",
                    "api",
                ],
                "deployment.logs",
            ),
            (
                &["deployment", "remove", "/tmp/repo", "--deployment-id", "d1"],
                "deployment.remove",
            ),
            (
                &[
                    "deployment",
                    "set-domain",
                    "--deployment-id",
                    "d1",
                    "--domain",
                    "web",
                ],
                "deployment.set_domain",
            ),
            (&["health", "containers"], "health.containers"),
            (&["health", "summary"], "health.summary"),
            (&["health", "repositories"], "health.repositories"),
            (&["health", "repository", "/tmp/repo"], "health.repository"),
            (
                &[
                    "health",
                    "history",
                    "--subject-kind",
                    "host",
                    "--subject-id",
                    "host",
                    "--metric",
                    "load_1",
                ],
                "health.history",
            ),
            (&["telegram", "list"], "telegram.list"),
            (
                &[
                    "telegram",
                    "link",
                    "--code",
                    "123456",
                    "--email",
                    "owner@example.test",
                ],
                "telegram.link",
            ),
            (
                &[
                    "telegram",
                    "subscribe",
                    "--chat-id",
                    "42",
                    "--scope",
                    "repository:r1",
                ],
                "telegram.subscribe",
            ),
            (
                &[
                    "telegram",
                    "unsubscribe",
                    "--chat-id",
                    "42",
                    "--scope",
                    "repository:r1",
                ],
                "telegram.unsubscribe",
            ),
            (&["repository", "list"], "repository.list"),
            (&["repository", "status", "/tmp/repo"], "repository.status"),
            (
                &["repository", "register", "/tmp/repo"],
                "repository.register",
            ),
            (
                &[
                    "repository",
                    "archive",
                    "r1111111111111111",
                    "--into",
                    "r2222222222222222",
                    "--note",
                    "Merged into target",
                ],
                "repository.archive",
            ),
            (
                &[
                    "repository",
                    "unarchive",
                    "r1111111111111111",
                    "--note",
                    "Restore repository",
                ],
                "repository.unarchive",
            ),
            (&["plan", "overview", "/tmp/repo"], "plan.overview"),
            (
                &[
                    "task",
                    "create",
                    "/tmp/repo",
                    "--title",
                    "Complete the access port",
                    "--kind",
                    "goal",
                ],
                "task.create",
            ),
            (
                &["task", "update", "p1", "--status", "in_progress"],
                "task.update",
            ),
            (
                &[
                    "task",
                    "search",
                    "/tmp/repo",
                    "--query",
                    "lost",
                    "--status",
                    "dropped",
                    "--limit",
                    "1",
                ],
                "task.search",
            ),
            (&["task", "history", "p1"], "task.history"),
            (
                &[
                    "release",
                    "create",
                    "/tmp/repo",
                    "--name",
                    "Preview",
                    "--kind",
                    "preview",
                ],
                "release.create",
            ),
            (
                &[
                    "release",
                    "update",
                    "--release-id",
                    "v1",
                    "--status",
                    "planned",
                ],
                "release.update",
            ),
            (&["release", "request", "/tmp/repo"], "release.request"),
            (
                &[
                    "release",
                    "deliver",
                    "--release-id",
                    "v1",
                    "--deployment-id",
                    "d1",
                ],
                "release.deliver",
            ),
            (
                &[
                    "decision",
                    "record",
                    "/tmp/repo",
                    "--aspect",
                    "architecture",
                    "--title",
                    "Keep one registry",
                    "--body",
                    "Use one typed operation registry for every client.",
                ],
                "decision.record",
            ),
            (
                &["decision", "tail", "/tmp/repo", "-n", "5"],
                "decision.tail",
            ),
            (
                &[
                    "decision",
                    "search",
                    "/tmp/repo",
                    "--query",
                    "typed registry",
                ],
                "decision.search",
            ),
            (
                &[
                    "decision",
                    "summarize",
                    "/tmp/repo",
                    "--body",
                    "This summary records the current durable direction.",
                    "--covers-through-seq",
                    "12",
                ],
                "decision.summarize",
            ),
            (
                &[
                    "event",
                    "wait",
                    "--cursor",
                    "42",
                    "--filter",
                    r#"{"filter_id":"health","categories":["health"],"deadline_at":"2026-09-05T00:00:00Z"}"#,
                ],
                "event.wait",
            ),
        ];

        let mut seen_routes = HashSet::new();
        for (args, expected) in cases {
            let (actual, _) = remote_invocation(args);
            assert_eq!(actual, *expected, "{args:?}");
            let route = devcoordinator2_api::OPERATIONS
                .iter()
                .flat_map(|operation| {
                    operation
                        .cli_routes
                        .iter()
                        .map(move |route| (operation, route))
                })
                .filter(|(_, route)| route.dispatch == CliDispatch::Protocol)
                .filter(|(_, route)| {
                    let route = route.route.split_whitespace().collect::<Vec<_>>();
                    args.starts_with(&route)
                })
                .max_by_key(|(_, route)| route.route.split_whitespace().count())
                .unwrap_or_else(|| panic!("{args:?} has no registered protocol CLI route"));
            assert_eq!(route.0.name, *expected, "{args:?}");
            seen_routes.insert(route.1.route);
        }
        let registered_routes = devcoordinator2_api::OPERATIONS
            .iter()
            .flat_map(|operation| operation.cli_routes)
            .filter(|route| route.dispatch == CliDispatch::Protocol)
            .map(|route| route.route)
            .collect::<HashSet<_>>();
        assert_eq!(seen_routes, registered_routes);
    }

    #[test]
    fn paths_repeated_checks_globals_and_nullable_moves_keep_meaning() {
        let cli = Cli::try_parse_from([
            "devcoordinator2",
            "test",
            "start",
            ".",
            "--check",
            "unit",
            "--check",
            "browser",
            "--tier",
            "development",
            "--format",
            "human",
            "--client",
            "codex",
            "--session",
            "task-7",
        ])
        .expect("parse globals");
        assert_eq!(cli.format, OutputFormat::Human);
        assert_eq!(cli.client_context().kind, ClientKind::Codex);
        assert_eq!(cli.client_context().session.as_deref(), Some("task-7"));
        let Invocation::Remote { operation, params } = cli.into_invocation().expect("mapping")
        else {
            panic!("remote expected")
        };
        assert_eq!(operation, "test.start");
        assert!(PathBuf::from(params["path"].as_str().expect("path")).is_absolute());
        assert_eq!(params["checks"], json!(["unit", "browser"]));
        assert_eq!(params["tier"], "development");

        let (_, list) = remote_invocation(&["deployment", "list"]);
        assert_eq!(list, json!({}));
        let (_, implicit) = remote_invocation(&["deployment", "apply", "/tmp/repo"]);
        assert_eq!(implicit, json!({"path":"/tmp/repo"}));
        let (_, cross_checked) = remote_invocation(&[
            "deployment",
            "apply",
            "/tmp/repo",
            "--name",
            "web",
            "--deployment-id",
            "d1",
        ]);
        assert_eq!(
            cross_checked,
            json!({"path":"/tmp/repo","name":"web","deployment_id":"d1"})
        );
        let (_, all_plans) = remote_invocation(&["plan", "overview", "--all"]);
        assert_eq!(all_plans, json!({}));
        let (_, moved) = remote_invocation(&[
            "task",
            "update",
            "p1",
            "--backlog",
            "--root",
            "--elaboration-complete",
        ]);
        assert_eq!(moved["release_id"], Value::Null);
        assert_eq!(moved["parent_task_id"], Value::Null);
        assert_eq!(moved["elaboration_needed"], false);

        let (_, feedback_state) = remote_invocation(&[
            "test",
            "evidence",
            "feedback",
            "state",
            "--run-id",
            "trun",
            "--feedback-id",
            "f1",
            "open",
        ]);
        assert_eq!(feedback_state["state"], "open");
        assert!(
            PathBuf::from(feedback_state["path"].as_str().expect("feedback path")).is_absolute()
        );

        let (_, clear) = remote_invocation(&[
            "deployment",
            "set-domain",
            "--deployment-id",
            "d1",
            "--clear",
            "--authenticated",
        ]);
        assert_eq!(clear["domain"], Value::Null);
        assert_eq!(clear["public"], false);

        let (_, event_wait) = remote_invocation(&[
            "event",
            "wait",
            "--cursor",
            "9",
            "--filter",
            r#"{"filter_id":"tests","categories":["test"]}"#,
            "--filter",
            r#"{"filter_id":"deployments","categories":["deployment"]}"#,
            "--limit",
            "25",
        ]);
        assert_eq!(event_wait["cursor"], 9);
        assert_eq!(event_wait["filters"].as_array().unwrap().len(), 2);
        assert_eq!(event_wait["limit"], 25);
    }

    #[test]
    fn special_local_invocations_remain_outside_daemon_dispatch() {
        assert!(matches!(
            invocation(&["daemon"]).unwrap(),
            Invocation::Daemon
        ));
        assert!(matches!(invocation(&["mcp"]).unwrap(), Invocation::Mcp));
        assert!(matches!(
            invocation(&["test", "event", "unsafe"]).unwrap(),
            Invocation::TestEvent {
                status: TestEventStatus::Unsafe
            }
        ));
        assert!(matches!(
            invocation(&["bug", "list"]).unwrap(),
            Invocation::OfflineBug {
                action: OfflineBugAction::List
            }
        ));
        assert!(matches!(
            invocation(&["bug", "close", "b123456789abc"]).unwrap(),
            Invocation::OfflineBug {
                action: OfflineBugAction::Close { .. }
            }
        ));
        assert!(matches!(
            invocation(&[
                "bug",
                "report",
                "--component",
                "daemon",
                "--summary",
                "Failed",
                "--expected",
                "Success",
                "--actual",
                "Failure",
                "--steps",
                "Run it"
            ])
            .unwrap(),
            Invocation::OfflineBug {
                action: OfflineBugAction::Report { .. }
            }
        ));
        assert!(matches!(
            invocation(&[
                "test",
                "artifact",
                "materialize",
                "/tmp/repo",
                "--run-id",
                "trun",
                "--check",
                "browser",
                "--destination",
                "/tmp/materialized"
            ])
            .unwrap(),
            Invocation::ArtifactMaterialize { .. }
        ));
        for (route, operation) in [
            ("bug report", "bug.report"),
            ("bug list", "bug.list"),
            ("bug close", "bug.close"),
        ] {
            let (definition, metadata) =
                devcoordinator2_api::operation_for_cli_route(route).expect("local CLI route");
            assert_eq!(definition.name, operation);
            assert_eq!(metadata.dispatch, CliDispatch::Local);
        }
    }

    #[test]
    fn invalid_mutual_combinations_and_bounds_fail_before_transport() {
        for args in [
            vec!["deployment", "logs", "/tmp/repo", "--deployment-id", "d1"],
            vec!["deployment", "set-domain", "--deployment-id", "d1"],
            vec![
                "deployment",
                "set-domain",
                "--deployment-id",
                "d1",
                "--domain",
                "web",
                "--clear",
            ],
            vec!["task", "update", "p1", "--backlog", "--release-id", "v1"],
            vec!["task", "update", "p1", "--root", "--parent-task-id", "p2"],
            vec![
                "test",
                "log",
                "range",
                "/tmp/repo",
                "--phase",
                "check",
                "--stream",
                "stdout",
                "--line-start",
                "1",
            ],
            vec![
                "test",
                "log",
                "range",
                "/tmp/repo",
                "--phase",
                "check",
                "--stream",
                "stdout",
                "--line-start",
                "1",
                "--line-end",
                "2",
                "--byte-start",
                "0",
                "--byte-end",
                "10",
            ],
            vec![
                "test",
                "artifact",
                "catalog",
                "/tmp/repo",
                "--run-id",
                "trun",
                "--check",
                "unit",
                "--manifest-sha256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ],
            vec![
                "test",
                "artifact",
                "materialize",
                "/tmp/repo",
                "--run-id",
                "trun",
                "--check",
                "unit",
                "--destination",
                "relative",
            ],
            vec!["decision", "tail", "/tmp/repo", "-n", "51"],
            vec![
                "event",
                "wait",
                "--filter",
                r#"{"filter_id":"health"}"#,
                "--limit",
                "0",
            ],
            vec!["event", "wait", "--filter", "not-json"],
        ] {
            assert!(invocation(&args).is_err(), "{args:?} should fail locally");
        }
        assert!(
            Cli::try_parse_from([
                "devcoordinator2",
                "deployment",
                "set-domain",
                "--deployment-id",
                "d1",
                "--domain",
                "web",
                "--public",
                "--authenticated",
            ])
            .is_err()
        );
    }

    #[test]
    fn response_rendering_preserves_v2_data_and_error_detail() {
        let success =
            ResponseEnvelope::success("request-1", json!({"repository_id":"r1","status":"ready"}))
                .expect("success");
        let mut json_output = Vec::new();
        render_response_to(
            &mut json_output,
            &success,
            OutputFormat::Json,
            Some("repository.status"),
        )
        .expect("render json");
        let decoded: Value = serde_json::from_slice(&json_output).expect("valid JSON");
        assert_eq!(decoded["protocol"], 2);
        assert_eq!(decoded["data"]["repository_id"], "r1");
        assert!(decoded.get("result").is_none());

        let mut human_output = Vec::new();
        render_response_to(
            &mut human_output,
            &success,
            OutputFormat::Human,
            Some("repository.status"),
        )
        .expect("render human");
        let human = String::from_utf8(human_output).expect("utf8");
        assert!(human.contains("repository.status:"));
        assert!(human.contains("\"repository_id\": \"r1\""));

        let failure = ResponseEnvelope::failure(
            "request-2",
            ProtocolError::new(ErrorCode::PermissionDenied, "administrator required")
                .with_detail("operation remains unchanged"),
        );
        let mut error_output = Vec::new();
        render_response_to(
            &mut error_output,
            &failure,
            OutputFormat::Human,
            Some("test.start"),
        )
        .expect("render error");
        let error = String::from_utf8(error_output).expect("utf8");
        assert!(error.contains("permission_denied"));
        assert!(error.contains("administrator required"));
        assert!(error.contains("operation remains unchanged"));
    }
}
