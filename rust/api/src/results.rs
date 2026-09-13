//! Typed operation results. The first migration checkpoint contains the shared
//! identity, repository, planning, and scalar evidence vocabulary; remaining
//! domain projections are filled in as their services are ported.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::params::{AccessRole, DecisionAspect, ReleaseKind, ReleaseStatus, TaskKind, TaskStatus};

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Running,
    Passed,
    Failed,
    #[serde(rename = "timed-out")]
    TimedOut,
    Cancelled,
    Interrupted,
    Superseded,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    Complete,
    Selected,
    Retry,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafStatus {
    Pending,
    Running,
    Reused,
    Passed,
    Failed,
    TimedOut,
    Invalidated,
    NotMeaningful,
    Cancelled,
    Unsafe,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    DeadlineExceeded,
    UserCancelled,
    Superseded,
    RunCancelled,
    DaemonInterrupted,
    UnsafeStop,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminationReason {
    OperatorCancelled,
    MemoryPressure,
    Superseded,
    TimedOut,
    Interrupted,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRole {
    Work,
    Preflight,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticValueType {
    Null,
    Boolean,
    Number,
    String,
    Json,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogPhase {
    Executor,
    Check,
    Discovery,
    Case,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedInvitation {
    pub email: String,
    pub user_id: String,
    pub accepted: bool,
    pub administrator: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhoAmI {
    pub local: bool,
    pub identity: Option<String>,
    pub user_id: Option<String>,
    pub administrator: bool,
    pub grants: BTreeMap<String, AccessRole>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserGrant {
    pub deployment_id: String,
    pub role: AccessRole,
    pub granted_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessUser {
    pub user_id: String,
    pub email: String,
    pub subject: Option<String>,
    pub display_name: Option<String>,
    pub administrator: bool,
    pub created_at: String,
    pub created_by: String,
    pub last_seen_at: Option<String>,
    pub grants: Vec<UserGrant>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationGrant {
    pub deployment_id: String,
    pub role: AccessRole,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub invitation_id: String,
    pub email: String,
    pub administrator: bool,
    pub created_at: String,
    pub created_by: String,
    pub expires_at: String,
    pub grants: Vec<InvitationGrant>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserList {
    pub users: Vec<AccessUser>,
    pub invitations: Vec<Invitation>,
    pub roles: Vec<AccessRole>,
    pub owners: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitedUser {
    pub invitation_id: String,
    pub email: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovedUser {
    pub email: String,
    pub removed_user: bool,
    pub removed_invitation: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSet {
    pub email: String,
    pub deployment_id: String,
    pub role: AccessRole,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRemoved {
    pub email: String,
    pub deployment_id: String,
    pub removed: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub worktree_id: String,
    pub worktree_path: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub repository_id: String,
    pub root_path: String,
    pub display_name: String,
    pub registered_at: String,
    pub registered_by_uid: u32,
    pub last_seen_at: String,
    pub archived_at: Option<String>,
    pub archived_by_uid: Option<u32>,
    pub archive_note: Option<String>,
    pub merged_into_repository_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryListRow {
    pub repository_id: String,
    pub root_path: String,
    pub display_name: String,
    pub registered_at: String,
    pub last_seen_at: String,
    pub archived_at: Option<String>,
    pub archived_by_uid: Option<u32>,
    pub archive_note: Option<String>,
    pub merged_into_repository_id: Option<String>,
    pub worktrees: Vec<Worktree>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryList {
    pub repositories: Vec<RepositoryListRow>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPresentation {
    pub repository_id: String,
    pub display_name: Option<String>,
    pub icon: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredRepository {
    pub repository_id: String,
    pub worktree_id: String,
    pub root_path: String,
    pub worktree_path: String,
    pub display_name: String,
    pub registered: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentTest {
    pub status: TestStatus,
    pub run_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryStatus {
    pub repository_id: String,
    pub root_path: String,
    pub display_name: String,
    pub registered_at: String,
    pub last_seen_at: String,
    pub archived_at: Option<String>,
    pub archived_by_uid: Option<u32>,
    pub archive_note: Option<String>,
    pub merged_into_repository_id: Option<String>,
    pub worktrees: Vec<Worktree>,
    pub worktree_id: String,
    pub current_test: Option<CurrentTest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestStarted {
    pub run_id: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub test: String,
    pub status: TestStatus,
    pub proof: ProofKind,
    pub selection: Vec<String>,
    pub origin_run_id: Option<String>,
    pub requested_tier: crate::params::ValidationTier,
    pub readiness_eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_run_id: Option<String>,
    pub unit: String,
    pub summary_ref: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogReference {
    pub run_id: String,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLogStreamSummary {
    pub log_ref: LogReference,
    pub bytes: u64,
    pub lines: u64,
    pub sha256: String,
    pub first_write_epoch_ms: Option<u64>,
    pub last_write_epoch_ms: Option<u64>,
    pub complete: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactReceipt {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProgress {
    pub waiting: u32,
    pub admitted: u32,
    pub executing: u32,
    pub finished: u32,
    pub queued_at_epoch_ms: u64,
    pub admitted_at_epoch_ms: Option<u64>,
    pub started_at_epoch_ms: Option<u64>,
    pub finished_at_epoch_ms: Option<u64>,
    pub capacity_wait_ms: u64,
    pub process_duration_ms: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionProgress>,
    pub id: String,
    pub status: LeafStatus,
    pub exit: DiagnosticExit,
    pub duration_ms: u64,
    pub streams: Vec<ExecutionLogStreamSummary>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckProjection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionProgress>,
    pub name: String,
    pub tier: crate::params::ValidationTier,
    pub role: CheckRole,
    pub status: LeafStatus,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub exit: DiagnosticExit,
    pub streams: Vec<ExecutionLogStreamSummary>,
    pub case_count: u32,
    pub artifacts: Vec<ArtifactReceipt>,
    pub artifacts_truncated: bool,
    pub retained_artifacts: Vec<RetainedArtifactReceipt>,
    pub retained_artifacts_truncated: bool,
    pub cases: Vec<CaseProjection>,
    pub cases_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticValue {
    #[serde(rename = "type")]
    pub value_type: DiagnosticValueType,
    pub sha256: String,
    pub byte_count: u64,
    pub preview: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureIndexEntry {
    pub check: Option<String>,
    pub case: Option<String>,
    pub status: LeafStatus,
    pub exit: DiagnosticExit,
    pub termination_reason: Option<TerminationReason>,
    pub source: Option<SourceLocation>,
    pub error_category: String,
    pub expected: Option<DiagnosticValue>,
    pub actual: Option<DiagnosticValue>,
    pub fingerprint: String,
    pub occurrences: u32,
    pub log_refs: Vec<LogReference>,
    pub origin: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCapacity {
    pub learned_capacity: Option<u32>,
    pub effective_capacity: Option<u32>,
    pub capacity_wait_count: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<crate::work_context::WorkAttribution>,
    pub schema_version: u8,
    pub run_id: String,
    pub test: String,
    pub status: TestStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub exit_code: Option<i32>,
    pub stdout_bytes_observed: u64,
    pub stderr_bytes_observed: u64,
    pub caller_uid: u32,
    pub client: String,
    pub proof: ProofKind,
    pub selection: Vec<String>,
    pub origin_run_id: Option<String>,
    pub requested_tier: crate::params::ValidationTier,
    pub readiness_eligible: bool,
    pub check_report_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_issue: Option<TestReportIssue>,
    pub log_catalog_ref: LogCatalogReference,
    pub termination_reason: Option<RunTerminationReason>,
    pub check_summary: Option<BTreeMap<String, u32>>,
    pub checks: Option<Vec<CheckProjection>>,
    pub checks_truncated: Option<bool>,
    pub failure_index: Option<Vec<FailureIndexEntry>>,
    pub failure_index_truncated: Option<bool>,
    pub source_changed: Option<bool>,
    pub execution_capacity: Option<ExecutionCapacity>,
    pub capacity_wait_count: Option<u64>,
    pub capacity: Option<Capacity>,
}

#[derive(Clone, Copy, Debug, JsonSchema, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestReportIssue {
    Missing,
    Invalid,
    Unreadable,
    IdentityMismatch,
    Incomplete,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalogReference {
    pub run_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestListRow {
    pub worktree_id: String,
    pub worktree_path: String,
    pub repository_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_source: Option<TestRepositorySource>,
    pub visual_evidence: VisualEvidenceSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earlier_visual_evidence: Option<EarlierVisualEvidence>,
    #[serde(flatten)]
    pub summary: TestSummary,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRepositorySource {
    pub key: String,
    pub name: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualEvidenceSummary {
    pub status: String,
    pub bundle_count: u32,
    pub image_count: u32,
    pub issue_count: u32,
    pub issues_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EarlierVisualEvidence {
    pub run_id: String,
    pub test: String,
    pub started_at: String,
    pub visual_evidence: VisualEvidenceSummary,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestList {
    pub runs: Vec<TestListRow>,
    #[serde(default)]
    pub next_worktree_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestHistoryRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<crate::work_context::WorkAttribution>,
    pub run_id: String,
    pub test: String,
    pub status: TestStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<RunTerminationReason>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestHistory {
    pub runs: Vec<TestHistoryRun>,
    pub next_before: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredEvidenceSummary {
    pub available: bool,
    pub formats: Vec<String>,
    pub count: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalogEntry {
    pub log_ref: LogReference,
    pub bytes: u64,
    pub lines: Option<u64>,
    pub first_byte_at: Option<String>,
    pub last_byte_at: Option<String>,
    pub complete: bool,
    pub truncated: bool,
    pub sha256: Option<String>,
    pub expires_at: Option<String>,
    pub depth_rank: Option<u32>,
    pub structured_evidence: StructuredEvidenceSummary,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalog {
    pub entries: Vec<LogCatalogEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSegment {
    pub line_start: u64,
    pub line_end: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: Option<String>,
    pub base64: Option<String>,
    pub rank: Option<u32>,
    pub fingerprint: Option<String>,
    pub occurrences: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogContent {
    pub segments: Vec<LogSegment>,
    pub snapshot_bytes: u64,
    pub snapshot_lines: u64,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSearch {
    pub matches: Vec<LogSegment>,
    pub snapshot_bytes: u64,
    pub snapshot_lines: u64,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogFailureContext {
    pub failures: Vec<FailureIndexEntry>,
    pub contexts: Vec<LogSegment>,
    pub snapshot_bytes: Option<u64>,
    pub snapshot_lines: Option<u64>,
    pub next_cursor: Option<String>,
    pub response_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionDefaults {
    pub max_age_seconds: u32,
    pub case_depth: u16,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub max_age_seconds: u32,
    pub case_depth: u16,
    pub defaults: RetentionDefaults,
    pub updated_at: String,
    pub updated_by: String,
    pub last_cleanup_at: Option<String>,
    pub last_cleanup_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_requested: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityAdjustment {
    pub event_id: u64,
    pub at: String,
    pub actor: String,
    pub reason: String,
    pub previous_capacity: u32,
    pub new_capacity: u32,
    pub cap: Option<u32>,
    pub p95_cpu_percent: Option<f64>,
    pub p95_memory_percent: Option<f64>,
    pub saturation_fraction: Option<f64>,
    pub epoch_seconds: Option<f64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub learned_capacity: u32,
    pub effective_capacity: u32,
    pub cap: Option<u32>,
    pub active: u32,
    pub waiting: u32,
    pub paused: bool,
    pub last_adjustment: Option<CapacityAdjustment>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Screenshot {
    Available(AvailableScreenshot),
    Unavailable(UnavailableScreenshot),
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvailableScreenshot {
    pub status: String,
    pub image_id: String,
    pub kind: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub captured_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnavailableScreenshot {
    pub status: String,
    pub kind: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceViewport {
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceAction {
    pub index: u32,
    pub action: String,
    pub outcome: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFinding {
    pub severity: String,
    pub rule: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReview {
    pub status: String,
    pub decision: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceScreenshots {
    pub viewport: Option<Screenshot>,
    pub full_page: Option<Screenshot>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCell {
    pub cell_id: String,
    pub review_cell_key: Option<String>,
    pub plan_index: Option<u32>,
    pub target_name: String,
    pub primary_journey: Option<String>,
    pub state_name: String,
    pub requested_path: Option<String>,
    pub final_path: Option<String>,
    pub viewport: EvidenceViewport,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub outcome: String,
    pub http_status: Option<u16>,
    pub source_binding_status: String,
    pub review: Option<EvidenceReview>,
    pub actions: Vec<EvidenceAction>,
    pub findings: Vec<EvidenceFinding>,
    pub screenshots: EvidenceScreenshots,
    pub formal_run_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCoverage {
    pub checked_pages: u32,
    pub planned_pages: u32,
    pub failed: bool,
    pub readiness_eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_coverage: Option<EvidenceRequiredCoverage>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRequiredCoverage {
    #[serde(alias = "declaredCount")]
    pub declared_count: u32,
    #[serde(alias = "satisfiedCount")]
    pub satisfied_count: u32,
    pub failed: bool,
    pub entries: Vec<EvidenceCoverageEntry>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCoverageEntry {
    #[serde(
        default,
        alias = "requirementId",
        skip_serializing_if = "Option::is_none"
    )]
    pub requirement_id: Option<String>,
    pub target: String,
    pub state: String,
    pub viewport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    pub status: String,
    #[serde(alias = "matchingCellIds")]
    pub matching_cell_ids: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBundle {
    pub formal_run_id: String,
    pub generated_at: String,
    pub browser: String,
    pub check: String,
    pub phase: String,
    pub case: Option<String>,
    pub coverage: EvidenceCoverage,
    pub cells: Vec<EvidenceCell>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackComment {
    pub comment_id: String,
    pub body: String,
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted: bool,
    pub can_edit: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    pub feedback_id: String,
    pub task_id: String,
    pub task_status: TaskStatus,
    pub state: String,
    pub run_id: String,
    pub check: String,
    pub phase: String,
    pub case: Option<String>,
    pub formal_run_id: String,
    pub cell_id: String,
    pub review_cell_key: Option<String>,
    pub image_id: String,
    pub screenshot_kind: String,
    pub screenshot_sha256: String,
    pub marks: Vec<crate::params::Mark>,
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    pub can_delete: bool,
    pub comments: Vec<FeedbackComment>,
    pub comments_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceIssue {
    pub check: String,
    pub phase: String,
    pub case: String,
    pub code: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceGet {
    pub repository_id: String,
    pub worktree_id: String,
    pub run_id: String,
    pub status: String,
    pub bundles: Vec<EvidenceBundle>,
    pub feedback: Vec<Feedback>,
    pub issues: Vec<EvidenceIssue>,
    pub issues_truncated: bool,
    pub image_count: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRunContext {
    pub repository_id: String,
    pub worktree_id: String,
    pub worktree_path: String,
    pub display_name: String,
    pub run_id: String,
    pub test: Option<String>,
    pub started_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLookup {
    pub context: EvidenceRunContext,
    pub evidence: EvidenceGet,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageChunk {
    pub image_id: String,
    pub mime: String,
    pub sha256: String,
    pub total_bytes: u64,
    pub offset: u64,
    pub bytes: u32,
    pub base64: String,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackMutation {
    pub feedback: Feedback,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackCreated {
    pub task_id: String,
    pub feedback_id: String,
    pub position: u32,
    pub feedback: Feedback,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSummary {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCatalog {
    pub repository_id: String,
    pub worktree_id: String,
    pub run_id: String,
    pub check: String,
    pub manifest_sha256: String,
    pub test: String,
    pub requested_tier: crate::params::ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub source_sha256: String,
    pub config_sha256: String,
    pub run_status: String,
    pub run_complete: bool,
    pub run_finished_at_epoch_ms: Option<u64>,
    pub run_metadata_sha256: String,
    pub artifacts: Vec<ArtifactSummary>,
    pub artifact: Option<ArtifactSummary>,
    pub entries: Vec<ArtifactEntry>,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactChunk {
    pub run_id: String,
    pub check: String,
    pub artifact: String,
    pub file: String,
    pub sha256: String,
    pub total_bytes: u64,
    pub offset: u64,
    pub bytes: u32,
    pub base64: String,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopTest {
    Cancelled {
        run_id: String,
        status: TestStatus,
    },
    AlreadyFinished {
        run_id: String,
        status: TestStatus,
        already_finished: bool,
    },
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentSource {
    Worktree,
    Checkout,
    Observed,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerClassification {
    ManagedTest,
    ManagedPreview,
    ManagedPermanent,
    ObservedCurrent,
    OrphanedManaged,
    Unmanaged,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredDeployment {
    pub name: String,
    pub source: DeploymentSource,
    pub deployment_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentListRow {
    pub deployment_id: String,
    pub repository_id: String,
    pub repository_name: Option<String>,
    pub name: String,
    pub source: DeploymentSource,
    pub state: String,
    pub domain: Option<String>,
    pub public: bool,
    pub current_generation: Option<u32>,
    pub route_port: Option<u16>,
    pub updated_at: String,
    pub ttl_expires_at: Option<String>,
    pub observed_only: bool,
    pub health: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentList {
    pub deployments: Vec<DeploymentListRow>,
    pub declared: Vec<DeclaredDeployment>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentBinding {
    pub kind: Option<String>,
    pub identity: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeService {
    pub name: String,
    pub role: String,
    pub state: String,
    pub desired_state: String,
    pub containers: u32,
    pub independent: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedService {
    pub service: String,
    pub generation: u32,
    pub container_id: String,
    pub image_id: Option<String>,
    pub exit_code: i32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub recorded_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    pub display_name: Option<String>,
    pub r#type: String,
    pub state: String,
    pub health: String,
    pub generation: Option<u32>,
    pub binding: ComponentBinding,
    pub port: Option<u16>,
    pub restarts: Option<u32>,
    pub owned: bool,
    pub independent_control: bool,
    pub last_error: Option<String>,
    pub services: Option<Vec<ComposeService>>,
    pub completed_services: Option<Vec<CompletedService>>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPreflight {
    pub repository_id: String,
    pub name: String,
    pub ready: bool,
    pub blockers: Vec<DeploymentBlocker>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentBlocker {
    pub component: String,
    pub code: crate::ErrorCode,
    pub file: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentReadiness {
    pub ready: bool,
    pub expected_components: Vec<String>,
    pub missing_components: Vec<String>,
    pub pending_apply: Option<bool>,
    pub blockers: Vec<DeploymentBlocker>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentStatus {
    pub deployment_id: String,
    pub repository_id: String,
    pub repository_name: Option<String>,
    pub name: String,
    pub source: DeploymentSource,
    pub state: String,
    pub health: Option<String>,
    pub current_generation: Option<u32>,
    pub previous_generation: Option<u32>,
    pub domain: Option<String>,
    pub route_port: Option<u16>,
    pub route_component: Option<String>,
    pub public: bool,
    pub ttl_expires_at: Option<String>,
    pub components: Vec<Component>,
    pub log_dir: Option<String>,
    pub observed_only: bool,
    pub native_project: Option<String>,
    pub observation_source: Option<String>,
    pub observed_at: Option<String>,
    pub unchanged: Option<bool>,
    pub rolled_back_from: Option<u32>,
    pub rolled_back_to: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<DeploymentReadiness>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentLog {
    pub deployment_id: Option<String>,
    pub component: String,
    pub tail: String,
    pub truncated_before_tail: Option<bool>,
    pub log_path: Option<String>,
    pub container_id: Option<String>,
    pub observed_only: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainChanged {
    pub deployment_id: String,
    pub domain: Option<String>,
    pub domain_source: Option<String>,
    pub declared_domain: Option<String>,
    pub route_port: Option<u16>,
    pub public: Option<bool>,
    pub observed_only: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentRemoved {
    pub deployment_id: String,
    pub removed: bool,
    pub data_deleted: bool,
    pub deleted_volumes: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerRow {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: String,
    pub repository_id: Option<String>,
    pub deployment_id: Option<String>,
    pub component: Option<String>,
    pub run_id: Option<String>,
    pub caller_uid: Option<u32>,
    pub client: Option<String>,
    pub ttl_seconds: Option<u64>,
    pub data: Option<String>,
    pub classification: ContainerClassification,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub pids: Option<u64>,
    pub container_layer_bytes: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerList {
    pub containers: Vec<ContainerRow>,
    pub counts: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovedContainer {
    pub container_id: String,
    pub removed: bool,
    pub classification: ContainerClassification,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostReconciliation {
    pub managed_cpu_percent: f64,
    pub daemon_cpu_percent: f64,
    pub other_cpu_percent: f64,
    pub managed_memory: u64,
    pub daemon_memory: u64,
    pub other_memory: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthHost {
    pub cpu_percent: f64,
    pub memory_total: u64,
    pub memory_used: u64,
    pub memory_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub fs_size: u64,
    pub fs_free: u64,
    pub fs_used: u64,
    pub ncpu: u32,
    pub reconciliation: HostReconciliation,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthStorage {
    pub fs_used: u64,
    pub managed_repositories: u64,
    pub devcoordinator_state: u64,
    pub docker_shared: u64,
    pub docker_images: u64,
    pub docker_build_cache: u64,
    pub docker_shared_volumes: u64,
    pub other: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnhealthyReason {
    pub component: String,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnhealthyDeployment {
    #[serde(flatten)]
    pub deployment: DeploymentListRow,
    pub reasons: Vec<UnhealthyReason>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alert {
    pub alert_key: String,
    pub kind: String,
    pub subject_kind: String,
    pub subject_id: String,
    pub severity: String,
    pub message: String,
    pub opened_at: String,
    pub last_seen_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingSummary {
    pub cpu_memory_seconds: u32,
    pub storage_seconds: u32,
    pub aggregate: String,
    pub retention_days: u32,
    pub stored_minutes: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSummary {
    pub host: HealthHost,
    pub storage: HealthStorage,
    pub unhealthy_deployments: Vec<UnhealthyDeployment>,
    pub active_tests: Vec<String>,
    pub container_counts: BTreeMap<String, u32>,
    pub alerts: Vec<Alert>,
    pub sampling: SamplingSummary,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryStorage {
    pub checkout: u64,
    pub test_scratch: u64,
    pub deployment_artifacts: u64,
    pub container_layers: u64,
    pub volumes: u64,
    pub postgres_data: u64,
    pub total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_layer: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_connections: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_wal_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_temp_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_database_bytes: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryHealthRow {
    pub repository_id: String,
    pub display_name: String,
    pub root_path: Option<String>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub storage_bytes: Option<u64>,
    pub storage: RepositoryStorage,
    pub health: String,
    pub deployments: Vec<DeploymentListRow>,
    pub trend_cpu: Vec<f64>,
    pub trend_memory: Vec<u64>,
    pub trend_storage: Vec<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSummary {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub storage_bytes: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedResourceSummary {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub storage: HealthStorage,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthRepositories {
    pub repositories: Vec<RepositoryHealthRow>,
    pub devcoordinator: ResourceSummary,
    pub shared_unattributed: SharedResourceSummary,
    pub host: HealthHost,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSample {
    pub cpu_percent: f64,
    pub cpu_usec_total: u64,
    pub memory_bytes: u64,
    pub memory_peak: u64,
    pub pids: u64,
    pub io_read_bytes_total: u64,
    pub io_write_bytes_total: u64,
    pub io_read: u64,
    pub io_write: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryComponentMetric {
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(flatten)]
    pub metric: MetricSample,
    pub storage: RepositoryStorage,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthRepository {
    pub repository_id: String,
    pub display_name: String,
    pub live: Option<MetricSample>,
    pub storage: Option<RepositoryStorage>,
    pub components: Vec<RepositoryComponentMetric>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPoint {
    pub minute: String,
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    pub samples: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthHistory {
    pub subject_kind: crate::params::MetricSubjectKind,
    pub subject_id: String,
    pub metric: crate::params::MetricName,
    pub minutes: u32,
    pub points: Vec<MetricPoint>,
    pub truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentReleaseSummary {
    pub name: String,
    pub kind: ReleaseKind,
    pub status: ReleaseStatus,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRepositoryRow {
    pub repository_id: String,
    pub display_name: String,
    #[serde(default)]
    pub root_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_source: Option<TestRepositorySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<RepositoryPresentation>,
    pub open_tasks: u32,
    pub loc_done: u64,
    pub loc_total: u64,
    pub current_release: Option<CurrentReleaseSummary>,
    pub preview_requested: bool,
    pub elaboration_request_count: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCollection {
    pub repositories: Vec<PlanRepositoryRow>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRelease {
    pub release_id: String,
    pub name: String,
    pub kind: ReleaseKind,
    pub status: ReleaseStatus,
    pub seq: u32,
    pub note: Option<String>,
    pub requested_at: Option<String>,
    pub delivered_at: Option<String>,
    pub url: Option<String>,
    pub port: Option<u16>,
    pub tasks_total: u32,
    pub tasks_done: u32,
    pub loc_total: u64,
    pub loc_done: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub release_id: Option<String>,
    pub seq: u32,
    pub position: u32,
    pub title: String,
    pub impact: Option<String>,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub estimated_loc: Option<u32>,
    pub elaboration_needed: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub release_id: String,
    pub name: String,
    pub requested_at: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionState {
    pub unsummarized_count: u32,
    pub summary_due: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDetail {
    pub repository_id: String,
    pub display_name: String,
    pub archived: bool,
    pub merged_into_repository_id: Option<String>,
    pub releases: Vec<PlanRelease>,
    pub tasks: Vec<PlanTask>,
    pub tasks_truncated: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
    pub preview_requested: Vec<PreviewRequest>,
    pub decisions: DecisionState,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlanOverview {
    Collection(PlanCollection),
    Detail(PlanDetail),
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCreated {
    pub task_id: String,
    pub repository_id: String,
    pub seq: u32,
    pub position: u32,
    pub status: TaskStatus,
    pub release_id: Option<String>,
    pub elaboration_needed: bool,
    pub preview_requested: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMutation {
    pub release_id: String,
    pub repository_id: Option<String>,
    pub seq: u32,
    pub name: String,
    pub kind: ReleaseKind,
    pub status: ReleaseStatus,
    pub note: Option<String>,
    pub requested_at: Option<String>,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDelivered {
    pub release_id: String,
    pub status: ReleaseStatus,
    pub delivered_at: String,
    pub url: Option<String>,
    pub port: Option<u16>,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub generation_number: u32,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSummary {
    pub body: String,
    pub covers_through_seq: u32,
    pub created_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTail {
    pub repository_id: String,
    pub display_name: String,
    pub summary: Option<DecisionSummary>,
    pub decisions: Vec<Decision>,
    pub has_more: bool,
    pub unsummarized_count: u32,
    pub summary_due: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSearch {
    pub repository_id: String,
    pub query: String,
    pub decisions: Vec<Decision>,
    pub has_more: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecorded {
    pub decision_id: String,
    pub seq: u32,
    pub r#ref: Option<String>,
    pub unsummarized_count: u32,
    pub summary_due: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSummarized {
    pub repository_id: String,
    pub covers_through_seq: u32,
    pub unsummarized_count: u32,
    pub summary_due: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Complete,
    Partial,
    Unavailable,
    Unobserved,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageCoverage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<UsageSnapshot>,
    pub state: CoverageState,
    pub has_gaps: bool,
    pub configured_collectors: u32,
    pub available_collectors: u32,
    pub contributing_collectors: u32,
    pub freshest_at_ms: Option<u64>,
    pub events: BTreeMap<String, u64>,
    pub token_observations: BTreeMap<String, u64>,
    pub unavailable_reasons: BTreeMap<String, u64>,
    pub database_schemas: Vec<u32>,
    pub taxonomy_versions: Vec<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSnapshot {
    pub updated_at_ms: Option<u64>,
    pub refreshing: bool,
    pub refresh_failed: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRepositoryRow {
    pub repository_id: String,
    pub display_name: String,
    pub range: crate::params::UsageRange,
    pub coverage: UsageCoverage,
    pub total_tokens: Option<u64>,
    pub model_requests: u64,
    pub tool_calls: u64,
    pub execution_wall_ms: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRepositories {
    pub range: crate::params::UsageRange,
    pub generated_at_ms: u64,
    pub repositories: Vec<UsageRepositoryRow>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTotals {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub model_requests: u64,
    pub tool_calls: u64,
    pub operations: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSeriesPoint {
    pub bucket_start_ms: u64,
    pub bucket_end_ms: u64,
    pub coverage: CoverageState,
    pub phases: BTreeMap<String, u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageActivity {
    pub phase: String,
    pub activity: String,
    pub total_tokens: u64,
    pub share: Option<f64>,
    pub operations: u64,
    pub provenance: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredTime {
    pub measured_ms: u64,
    pub unknown_intervals: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseTime {
    pub phase: String,
    pub measured_ms: u64,
    pub unknown_intervals: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTime {
    pub request_to_delivery: MeasuredTime,
    pub execution_wall: MeasuredTime,
    pub summed_agent_active: MeasuredTime,
    pub phases: Vec<PhaseTime>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcome {
    pub outcome: String,
    pub count: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFamily {
    pub family: String,
    pub count: u64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTools {
    pub outcomes: Vec<ToolOutcome>,
    pub families: Vec<ToolFamily>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSemantics {
    pub tokens: String,
    pub time: String,
    pub coverage: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRepository {
    pub repository_id: String,
    pub display_name: String,
    pub range: crate::params::UsageRange,
    pub generated_at_ms: u64,
    pub coverage: UsageCoverage,
    pub totals: UsageTotals,
    pub series: Vec<UsageSeriesPoint>,
    pub activities: Vec<UsageActivity>,
    pub time: UsageTime,
    pub tools: UsageTools,
    pub semantics: UsageSemantics,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextRelease {
    pub release_id: String,
    pub name: String,
    pub status: ReleaseStatus,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressRepositoryRow {
    pub repository_id: String,
    pub display_name: String,
    pub open_tasks: u32,
    pub tasks_done: u32,
    pub planned_lines_done: u64,
    pub planned_lines_total: u64,
    pub next_release: Option<NextRelease>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressRepositories {
    pub repositories: Vec<ProgressRepositoryRow>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressWindow {
    pub bucket_ms: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub comparison_start_ms: u64,
    pub timezone: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressScope {
    pub tasks_total: u32,
    pub tasks_done: u32,
    pub planned_lines_total: u64,
    pub planned_lines_done: u64,
    pub unestimated_open_tasks: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressSeriesPoint {
    pub bucket_start_ms: u64,
    pub bucket_end_ms: u64,
    pub tasks_completed: u32,
    pub tasks_created: u32,
    pub tasks_reopened: u32,
    pub planned_lines_completed: i64,
    pub planned_lines_added: i64,
    pub scope_lines_changed: i64,
    pub test_runs: u32,
    pub tests_passed: u32,
    pub test_pass_rate: Option<f64>,
    pub total_tokens: Option<u64>,
    pub token_coverage: CoverageState,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressTotals {
    pub tasks_completed: u32,
    pub tasks_created: u32,
    pub tasks_reopened: u32,
    pub planned_lines_completed: i64,
    pub planned_lines_added: i64,
    pub scope_lines_changed: i64,
    pub test_runs: u32,
    pub tests_passed: u32,
    pub test_pass_rate: Option<f64>,
    pub total_tokens: Option<u64>,
    pub tokens_per_completed_task: Option<f64>,
    pub tokens_per_planned_line: Option<f64>,
    pub tasks_completed_per_day: f64,
    pub tasks_created_per_day: f64,
    pub tests_per_completed_task: Option<f64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressComparison {
    pub current: ProgressTotals,
    pub previous: ProgressTotals,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForecastVelocity {
    pub tasks: u32,
    pub planned_lines: u64,
    pub lookback_days: f64,
    pub tasks_per_day: f64,
    pub planned_lines_per_day: f64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forecast {
    pub release: Option<NextRelease>,
    pub remaining_tasks: u32,
    pub remaining_planned_lines: u64,
    pub unestimated_tasks: u32,
    pub velocity: ForecastVelocity,
    pub target_date_recorded: bool,
    pub as_of_ms: u64,
    pub state: String,
    pub reason: Option<String>,
    pub likely_at_ms: Option<u64>,
    pub earliest_at_ms: Option<u64>,
    pub latest_at_ms: Option<u64>,
    pub confidence_percent: Option<u8>,
    pub confidence: Option<String>,
    pub drivers: Option<Vec<String>>,
    pub explanation: String,
    pub assumptions: Option<Vec<String>>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWork {
    pub task_id: String,
    pub title: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub estimated_loc: Option<u32>,
    pub elaboration_needed: bool,
    pub unblock_condition: Option<String>,
    pub reopened: bool,
    pub reopen_note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCoverage {
    pub state: CoverageState,
    pub completed_with_estimate: u32,
    pub completed_total: u32,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCoverage {
    pub state: CoverageState,
    pub recorded_runs: u32,
    pub history_sources: u32,
    pub unavailable_sources: u32,
    pub earliest_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCoverage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<UsageSnapshot>,
    pub state: CoverageState,
    pub has_gaps: bool,
    pub configured_collectors: u32,
    pub available_collectors: u32,
    pub contributing_collectors: u32,
    pub freshest_at_ms: Option<u64>,
    pub unavailable_reasons: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressCoverage {
    pub state: CoverageState,
    pub plan: PlanCoverage,
    pub tests: TestCoverage,
    pub tokens: TokenCoverage,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressSemantics {
    pub tasks: String,
    pub lines: String,
    pub lines_added: String,
    pub tests: String,
    pub tokens: String,
    pub forecast: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressRepository {
    pub repository_id: String,
    pub display_name: String,
    pub period: crate::params::ProgressPeriod,
    pub generated_at_ms: u64,
    pub window: ProgressWindow,
    pub scope: ProgressScope,
    pub series: Vec<ProgressSeriesPoint>,
    pub comparison: ProgressComparison,
    pub forecast: Forecast,
    pub release_work: Vec<ReleaseWork>,
    pub coverage: ProgressCoverage,
    pub semantics: ProgressSemantics,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramLinked {
    pub chat_id: i64,
    pub email: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramSubscription {
    pub chat_id: i64,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramChat {
    pub chat_id: i64,
    pub email: String,
    pub label: Option<String>,
    pub linked_at: String,
    pub subscriptions: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramList {
    pub configured: bool,
    pub chats: Vec<TelegramChat>,
    pub outbox_pending: u32,
    pub last_poll_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugCorrelations {
    pub run_id: Option<String>,
    pub deployment_id: Option<String>,
    pub component: Option<String>,
    pub repository_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugRecord {
    pub component: String,
    pub summary: String,
    pub expected: String,
    pub actual: String,
    pub steps: String,
    pub correlations: BugCorrelations,
    pub bug_id: String,
    pub opened_at: String,
    pub last_seen_at: String,
    pub occurrences: u32,
    pub reporter: String,
    pub duplicate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugList {
    pub bugs: Vec<BugRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BugClosed {
    pub bug_id: String,
    pub closed: bool,
    pub component: Option<String>,
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified: Option<bool>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestOwnedEvent {
    pub kind: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub run_id: String,
    pub test: String,
    pub status: Option<TestStatus>,
    pub exit_code: Option<i32>,
    pub duration_seconds: Option<f64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentOwnedEvent {
    pub kind: String,
    pub repository_id: Option<String>,
    pub deployment_id: String,
    pub component: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningOwnedEvent {
    pub kind: String,
    pub repository_id: String,
    pub subject_kind: String,
    pub subject_id: String,
    pub status: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthOwnedEvent {
    pub kind: String,
    pub repository_id: Option<String>,
    pub deployment_id: Option<String>,
    pub subject_kind: String,
    pub subject_id: String,
    pub severity: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackOwnedEvent {
    pub kind: String,
    pub repository_id: String,
    pub feedback_id: String,
    pub task_id: String,
    pub comment_id: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtherOwnedEvent {
    pub kind: String,
    pub repository_id: Option<String>,
    pub deployment_id: Option<String>,
    pub subject_kind: String,
    pub subject_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(tag = "category", content = "data", rename_all = "snake_case")]
pub enum OwnedEvent {
    Test(TestOwnedEvent),
    Deployment(DeploymentOwnedEvent),
    Planning(PlanningOwnedEvent),
    Health(HealthOwnedEvent),
    Feedback(FeedbackOwnedEvent),
    Other(OtherOwnedEvent),
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedEventRecord {
    pub cursor: u64,
    pub occurred_at: String,
    pub event: OwnedEvent,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventDelivery {
    pub filter_ids: Vec<String>,
    pub event: OwnedEventRecord,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatDue {
    pub filter_id: String,
    pub deadline_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventWaitResult {
    pub cursor: u64,
    pub events: Vec<EventDelivery>,
    pub heartbeat_due: Vec<HeartbeatDue>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElaborationRequest {
    pub task_id: String,
    pub title: String,
    pub outcome: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub requested_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub task_id: String,
    pub repository_id: String,
    pub parent_task_id: Option<String>,
    pub release_id: Option<String>,
    pub seq: u32,
    pub position: u32,
    pub title: String,
    pub outcome: String,
    pub impact: Option<String>,
    pub unblock_condition: Option<String>,
    pub verification: Option<String>,
    pub technical_note: Option<String>,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub estimated_loc: Option<u32>,
    pub elaboration_needed: bool,
    pub created_at: String,
    pub created_by: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskMutation {
    pub task_id: String,
    pub repository_id: String,
    pub seq: u32,
    pub position: u32,
    pub title: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub release_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub estimated_loc: Option<u32>,
    pub elaboration_needed: bool,
    pub preview_requested: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEvent {
    pub event: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub actor: String,
    pub at: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSearch {
    pub tasks: Vec<TaskSearchHit>,
    pub next_sequence: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSearchHit {
    pub task_id: String,
    pub sequence: u64,
    pub title: String,
    pub status: crate::params::TaskStatus,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskHistory {
    pub task: Task,
    pub events: Vec<PlanEvent>,
    pub events_truncated: bool,
    pub elaboration_requests: Vec<ElaborationRequest>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub release_id: String,
    pub repository_id: String,
    pub seq: u32,
    pub name: String,
    pub kind: ReleaseKind,
    pub status: ReleaseStatus,
    pub note: Option<String>,
    pub requested_at: Option<String>,
    pub delivered_at: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision_id: String,
    pub seq: u32,
    pub r#ref: Option<String>,
    pub aspect: DecisionAspect,
    pub title: String,
    pub body: String,
    pub technical_note: Option<String>,
    pub superseded_by: Option<String>,
    pub created_at: String,
    pub created_by: String,
}
