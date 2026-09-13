//! Strict operation inputs. Cross-field/domain validation is performed by the
//! owning service after Serde has rejected unknown fields and wrong primitive types.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessRole {
    Access,
    Viewer,
    Operator,
    Administrator,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationTier {
    Development,
    PreMerge,
    Release,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogPhase {
    Executor,
    Check,
    Discovery,
    Case,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Goal,
    Stub,
    Improvement,
    UserFeedback,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Planned,
    InProgress,
    Done,
    Dropped,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseKind {
    Preview,
    Release,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    Planned,
    Requested,
    Delivered,
    Dropped,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAspect {
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

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackState {
    Open,
    Resolved,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
pub enum UsageRange {
    #[serde(rename = "24h")]
    Hours24,
    #[serde(rename = "7d")]
    Days7,
    #[serde(rename = "30d")]
    Days30,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPeriod {
    Hour,
    Day,
    Week,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSubjectKind {
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

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricName {
    CpuPercent,
    MemoryBytes,
    Pids,
    StorageBytes,
    MemoryUsed,
    Load1,
    PgConnections,
    PgWalBytes,
    PgTempBytes,
    PgDatabaseBytes,
    IoRead,
    IoWrite,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptInvitation {
    #[schemars(email)]
    pub email: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationGrant {
    #[schemars(regex(pattern = r"^d[0-9a-f]+$"))]
    pub deployment_id: String,
    pub role: AccessRole,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteUser {
    #[schemars(email)]
    pub email: String,
    #[serde(default)]
    pub administrator: bool,
    #[serde(default)]
    pub grants: Vec<InvitationGrant>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailOnly {
    #[schemars(email)]
    pub email: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetGrant {
    #[schemars(email)]
    pub email: String,
    #[schemars(regex(pattern = r"^d[0-9a-f]+$"))]
    pub deployment_id: String,
    pub role: AccessRole,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveGrant {
    #[schemars(email)]
    pub email: String,
    #[schemars(length(min = 1))]
    pub deployment_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathOnly {
    #[schemars(length(min = 1))]
    pub path: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestHistory {
    pub path: String,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default = "default_test_history_limit")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u16,
}

fn default_test_history_limit() -> u16 {
    20
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryList {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPresentationUpdate {
    #[schemars(regex(pattern = r"^r[0-9a-f]{16}$"))]
    pub repository_id: String,
    #[schemars(length(min = 1, max = 80))]
    pub display_name: Option<String>,
    #[schemars(regex(
        pattern = r"^(folder|code|app-window|world|rocket|database|device-desktop|device-mobile|tools|flask|palette|star|plane|book|chart-bar|shield)$"
    ))]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRepository {
    #[schemars(regex(pattern = r"^r[0-9a-f]{16}$"))]
    pub repository_id: String,
    #[schemars(regex(pattern = r"^r[0-9a-f]{16}$"))]
    pub merged_into_repository_id: String,
    #[schemars(length(min = 3, max = 500))]
    pub note: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnarchiveRepository {
    #[schemars(regex(pattern = r"^r[0-9a-f]{16}$"))]
    pub repository_id: String,
    #[schemars(length(min = 3, max = 500))]
    pub note: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartTest {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default = "release_tier")]
    pub tier: ValidationTier,
}

fn release_tier() -> ValidationTier {
    ValidationTier::Release
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryTest {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub test: Option<String>,
    #[schemars(length(min = 1))]
    pub run_id: String,
    #[schemars(length(min = 1))]
    pub check: String,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSelector {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub phase: Option<LogPhase>,
    #[serde(default)]
    pub case: Option<String>,
    #[serde(default)]
    pub stream: Option<LogStream>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalog {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub phase: Option<LogPhase>,
    #[serde(default)]
    pub case: Option<String>,
    #[serde(default)]
    pub stream: Option<LogStream>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
    #[serde(default = "default_catalog_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u16,
}

fn default_catalog_limit() -> u16 {
    100
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogTail {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
    #[serde(default = "default_tail_lines")]
    #[schemars(range(min = 1, max = 5000))]
    pub lines: u16,
    #[serde(default = "default_log_bytes")]
    #[schemars(range(min = 1, max = 49152))]
    pub max_bytes: u32,
}

fn default_tail_lines() -> u16 {
    50
}

fn default_log_bytes() -> u32 {
    32_768
}

fn max_log_bytes() -> u32 {
    49_152
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSearch {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
    #[schemars(length(min = 1, max = 4096))]
    pub text: String,
    #[serde(default = "default_search_matches")]
    #[schemars(range(min = 1, max = 100))]
    pub max_matches: u16,
    #[serde(default = "default_context_lines")]
    #[schemars(range(max = 100))]
    pub context_lines: u16,
    #[serde(default = "default_log_bytes")]
    #[schemars(range(min = 1, max = 49152))]
    pub max_bytes: u32,
}

fn default_search_matches() -> u16 {
    20
}

fn default_context_lines() -> u16 {
    2
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRange {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
    #[serde(default)]
    pub line_start: Option<u64>,
    #[serde(default)]
    pub line_end: Option<u64>,
    #[serde(default)]
    pub byte_start: Option<u64>,
    #[serde(default)]
    pub byte_end: Option<u64>,
    #[serde(default = "max_log_bytes")]
    #[schemars(range(min = 1, max = 49152))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogFailureContext {
    #[schemars(length(min = 1))]
    pub path: String,
    #[serde(default)]
    pub run_id: Option<String>,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4096))]
    pub cursor: Option<String>,
    #[serde(default = "default_search_matches")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u16,
    #[serde(default = "default_context_lines")]
    #[schemars(range(max = 100))]
    pub context_lines: u16,
    #[serde(default = "default_log_bytes")]
    #[schemars(range(min = 1, max = 49152))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRetention {
    #[schemars(range(min = 1, max = 315360000))]
    pub max_age_seconds: u32,
    #[schemars(range(min = 1, max = 65535))]
    pub case_depth: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReference {
    #[schemars(length(min = 1))]
    pub path: String,
    #[schemars(regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"))]
    pub run_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLookup {
    #[schemars(regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"))]
    pub run_id: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub image_id: Option<String>,
    #[schemars(regex(pattern = r"^w[0-9a-f]{16}$"))]
    pub worktree_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceImage {
    #[schemars(length(min = 1))]
    pub path: String,
    #[schemars(regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$"))]
    pub run_id: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub image_id: String,
    #[serde(default)]
    #[schemars(range(max = 16777216))]
    pub offset: u32,
    #[serde(default = "default_image_bytes")]
    #[schemars(range(min = 1, max = 184320))]
    pub max_bytes: u32,
}

fn default_image_bytes() -> u32 {
    184_320
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    #[schemars(range(min = 0.0, max = 1.0))]
    pub x: f64,
    #[schemars(range(min = 0.0, max = 1.0))]
    pub y: f64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mark {
    Pin {
        id: String,
        color: String,
        x: f64,
        y: f64,
    },
    Text {
        id: String,
        color: String,
        x: f64,
        y: f64,
        text: String,
    },
    Rectangle {
        id: String,
        color: String,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    Arrow {
        id: String,
        color: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    },
    Freehand {
        id: String,
        color: String,
        points: Vec<Point>,
    },
    Highlight {
        id: String,
        color: String,
        points: Vec<Point>,
    },
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateFeedback {
    #[schemars(length(min = 1))]
    pub path: String,
    pub run_id: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub image_id: String,
    #[schemars(length(min = 3, max = 2000))]
    pub body: String,
    #[schemars(length(min = 1, max = 64))]
    pub marks: Vec<Mark>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackReply {
    pub path: String,
    pub run_id: String,
    pub feedback_id: String,
    #[schemars(length(min = 3, max = 2000))]
    pub body: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackEdit {
    pub path: String,
    pub run_id: String,
    pub feedback_id: String,
    pub comment_id: String,
    #[schemars(length(min = 3, max = 2000))]
    pub body: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackStateChange {
    pub path: String,
    pub run_id: String,
    pub feedback_id: String,
    pub state: FeedbackState,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackDelete {
    pub path: String,
    pub run_id: String,
    pub feedback_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCatalog {
    pub path: String,
    pub run_id: String,
    pub check: String,
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default)]
    pub manifest_sha256: Option<String>,
    #[serde(default)]
    #[schemars(range(max = 4096))]
    pub offset: u32,
    #[serde(default = "default_catalog_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile {
    pub path: String,
    pub run_id: String,
    pub check: String,
    pub artifact: String,
    #[schemars(length(min = 1, max = 512))]
    pub file: String,
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub manifest_sha256: String,
    #[serde(default)]
    #[schemars(range(max = 1073741824))]
    pub offset: u64,
    #[serde(default = "default_image_bytes")]
    #[schemars(range(min = 1, max = 184320))]
    pub max_bytes: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopTest {
    pub path: String,
    #[serde(default)]
    #[schemars(length(min = 3, max = 256))]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCapacity {
    pub cap: CapacityCap,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CapacityCap {
    Value(#[schemars(range(min = 1, max = 65535))] u16),
    Null,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentList {
    #[serde(default)]
    pub path: Option<String>,
}

macro_rules! deployment_params {
    ($name:ident { $($extra:tt)* }) => {
        #[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            #[serde(default)]
            pub deployment_id: Option<String>,
            #[serde(default)]
            pub path: Option<String>,
            #[serde(default)]
            pub name: Option<String>,
            $($extra)*
        }
    };
}

deployment_params!(DeploymentReference {});
deployment_params!(SetComposeEnvAuthorization {
    pub file: String,
    pub authorized: bool,
    pub expected_revision: String,
});
deployment_params!(DeploymentControl {
    #[serde(default)]
    pub component: Option<String>,
});
deployment_params!(DeploymentLogs {
    pub component: String,
    #[serde(default = "default_deployment_tail")]
    #[schemars(range(min = 1, max = 5000))]
    pub tail_lines: u16,
});
deployment_params!(RemoveDeployment {
    #[serde(default)]
    pub delete_data: bool,
});

fn default_deployment_tail() -> u16 {
    200
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDomain {
    pub deployment_id: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub public: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthHistory {
    pub subject_kind: MetricSubjectKind,
    #[schemars(length(min = 1))]
    pub subject_id: String,
    pub metric: MetricName,
    #[serde(default = "default_history_minutes")]
    #[schemars(range(min = 1, max = 43200))]
    pub minutes: u32,
    #[serde(default)]
    #[schemars(range(min = 2, max = 1440))]
    pub points: Option<u16>,
}

fn default_history_minutes() -> u32 {
    60
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveContainer {
    #[schemars(length(equal = 64))]
    pub container_id: String,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReference {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSearch {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    #[schemars(length(max = 256))]
    pub query: String,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub after_sequence: u64,
    #[serde(default = "task_search_limit")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u16,
}
fn task_search_limit() -> u16 {
    50
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskHistory {
    pub task_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreate {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[schemars(length(min = 3, max = 120))]
    pub title: String,
    pub kind: TaskKind,
    #[serde(default)]
    #[schemars(length(min = 10, max = 2000))]
    pub outcome: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 2000))]
    pub impact: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 2000))]
    pub unblock_condition: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 2000))]
    pub verification: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 4000))]
    pub technical_note: Option<String>,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub release_id: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 1000000))]
    pub estimated_loc: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdate {
    pub task_id: String,
    #[serde(default)]
    #[schemars(length(min = 3, max = 120))]
    pub title: Option<String>,
    #[serde(default)]
    #[schemars(length(min = 10, max = 2000))]
    pub outcome: Option<String>,
    #[serde(default)]
    pub impact: Option<String>,
    #[serde(default)]
    pub unblock_condition: Option<String>,
    #[serde(default)]
    pub verification: Option<String>,
    #[serde(default)]
    pub technical_note: Option<String>,
    #[serde(default)]
    pub estimated_loc: Option<u32>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default, deserialize_with = "deserialize_patch")]
    pub release_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_patch")]
    pub parent_task_id: Option<Option<String>>,
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub elaboration_needed: Option<bool>,
    #[serde(default)]
    #[schemars(length(min = 1, max = 500))]
    pub note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCreate {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[schemars(length(min = 3, max = 120))]
    pub name: String,
    pub kind: ReleaseKind,
    #[serde(default)]
    #[schemars(length(min = 1, max = 500))]
    pub note: Option<String>,
    #[serde(default)]
    pub seq: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseUpdate {
    pub release_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub seq: Option<u32>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub status: Option<ReleaseStatus>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDeliver {
    pub release_id: String,
    pub deployment_id: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTail {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub aspect: Option<DecisionAspect>,
    #[serde(default = "default_decision_limit")]
    pub n: u8,
    #[serde(default)]
    pub before_seq: Option<u32>,
}

fn default_decision_limit() -> u8 {
    10
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSearch {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[schemars(length(min = 1, max = 200))]
    pub query: String,
    #[serde(default)]
    pub aspect: Option<DecisionAspect>,
    #[serde(default = "default_decision_limit")]
    pub n: u8,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    pub aspect: DecisionAspect,
    #[schemars(length(min = 3, max = 120))]
    pub title: String,
    #[schemars(length(min = 10, max = 4000))]
    pub body: String,
    #[serde(default)]
    pub technical_note: Option<String>,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub supersedes: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSummarize {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[schemars(length(min = 10, max = 16000))]
    pub body: String,
    pub covers_through_seq: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRepositories {
    #[serde(default)]
    pub wait_for_refresh: bool,
    #[serde(default = "default_usage_range")]
    pub range: UsageRange,
}

fn default_usage_range() -> UsageRange {
    UsageRange::Hours24
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRepository {
    #[serde(default)]
    pub wait_for_refresh: bool,
    pub repository_id: String,
    #[serde(default = "default_usage_range")]
    pub range: UsageRange,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressRepository {
    #[serde(default)]
    pub wait_for_refresh: bool,
    pub repository_id: String,
    #[serde(default = "default_progress_period")]
    pub period: ProgressPeriod,
}

fn default_progress_period() -> ProgressPeriod {
    ProgressPeriod::Day
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramLink {
    pub code: String,
    #[serde(default)]
    #[schemars(email)]
    pub email: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramSubscription {
    pub chat_id: i64,
    pub scope: String,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugCorrelations {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugReport {
    #[schemars(length(min = 1, max = 64))]
    pub component: String,
    #[schemars(length(min = 1, max = 200))]
    pub summary: String,
    #[schemars(length(max = 2000))]
    pub expected: String,
    #[schemars(length(max = 2000))]
    pub actual: String,
    #[schemars(length(max = 4000))]
    pub steps: String,
    #[serde(default)]
    pub correlations: BugCorrelations,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugClose {
    #[schemars(regex(pattern = r"^b[0-9a-f]{12}$"))]
    pub bug_id: String,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    Test,
    Deployment,
    Planning,
    Health,
    Feedback,
    Other,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventFilter {
    #[schemars(regex(pattern = r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$"))]
    pub filter_id: String,
    #[serde(default)]
    #[schemars(length(max = 6))]
    pub categories: Vec<EventCategory>,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub kinds: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub repository_ids: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub deployment_ids: Vec<String>,
    #[serde(default)]
    pub deadline_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventWait {
    #[serde(default)]
    pub cursor: Option<u64>,
    #[schemars(length(min = 1, max = 32))]
    pub filters: Vec<EventFilter>,
    #[serde(default = "default_event_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u16,
}

fn default_event_limit() -> u16 {
    100
}

/// Used only for private access-guard filtering after public input validation.
pub type RepositoryAllowlist = BTreeMap<String, bool>;

fn deserialize_patch<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_fields_and_keeps_catalog_filters_optional() {
        assert!(serde_json::from_value::<Empty>(serde_json::json!({"extra": true})).is_err());
        let catalog: LogCatalog = serde_json::from_value(serde_json::json!({"path": "/x"}))
            .expect("catalog may enumerate without phase or stream");
        assert!(catalog.phase.is_none() && catalog.stream.is_none());
    }

    #[test]
    fn capacity_distinguishes_clear_from_missing() {
        let clear: SetCapacity = serde_json::from_value(serde_json::json!({"cap": null})).unwrap();
        assert_eq!(clear.cap, CapacityCap::Null);
        assert!(serde_json::from_value::<SetCapacity>(serde_json::json!({})).is_err());
    }

    #[test]
    fn task_patch_distinguishes_missing_clear_and_value() {
        let missing: TaskUpdate =
            serde_json::from_value(serde_json::json!({"task_id":"p1"})).unwrap();
        let clear: TaskUpdate =
            serde_json::from_value(serde_json::json!({"task_id":"p1","release_id":null})).unwrap();
        let value: TaskUpdate =
            serde_json::from_value(serde_json::json!({"task_id":"p1","release_id":"v1"})).unwrap();
        assert_eq!(missing.release_id, None);
        assert_eq!(clear.release_id, Some(None));
        assert_eq!(value.release_id, Some(Some("v1".into())));
    }
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestList {
    #[serde(default)]
    pub after_worktree_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
}
