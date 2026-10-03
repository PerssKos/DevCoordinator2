//! Shared storage inventory and exact-target cleanup contracts.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CACHE_IDLE_SECONDS: u64 = 3 * 86_400;
pub const DATA_IDLE_SECONDS: u64 = 14 * 86_400;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Container,
    Image,
    BuildCache,
    Network,
    Volume,
    Mount,
    BackingDirectory,
    BuildOutput,
    DependencyCache,
    Worktree,
    Backup,
    Evidence,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    Safe,
    InUse,
    Protected,
    NeedsReview,
    Observing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Rebuildable,
    PermanentData,
    SourceWorktree,
    RecoveryCopy,
    RetainedEvidence,
    RuntimeResource,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub artifact_id: String,
    pub revision: u64,
    pub repository_id: Option<String>,
    pub repository_name: Option<String>,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    #[serde(default)]
    pub accounting_id: String,
    pub name: String,
    pub kind: Kind,
    pub effect: Effect,
    pub ownership: String,
    pub filesystem_id: Option<String>,
    pub allocated_bytes: Option<u64>,
    pub last_used_at_ms: Option<u64>,
    pub observed_since_ms: u64,
    pub verified_at_ms: Option<u64>,
    pub eligible_at_ms: Option<u64>,
    pub safety: Safety,
    pub reasons: Vec<String>,
    pub protected: bool,
    pub deletable: bool,
    pub automatic_eligible: bool,
    pub dependencies: Vec<String>,
    pub aliases: Vec<String>,
    pub removed_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Filesystem {
    pub filesystem_id: String,
    pub label: String,
    pub capacity_bytes: u64,
    pub available_bytes: u64,
    pub measured_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub repository_id: Option<String>,
    pub filesystem_id: Option<String>,
    pub kind: Option<Kind>,
    pub safety: Option<Safety>,
    pub query: Option<String>,
    #[serde(default)]
    pub include_removed: bool,
    #[serde(default)]
    pub offset: u32,
    pub limit: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub revision: u64,
    pub artifacts: Vec<Artifact>,
    pub filesystems: Vec<Filesystem>,
    pub last_scan_at_ms: Option<u64>,
    pub coverage_gaps: Vec<String>,
    pub next_offset: Option<u32>,
    pub total: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub repository_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub repository_id: Option<String>,
    pub revision: u64,
    pub automatic: bool,
    pub cache_idle_seconds: u64,
    pub data_idle_seconds: u64,
    pub minimum_verified_backups: u32,
    pub inherited: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicySet {
    pub repository_id: Option<String>,
    pub expected_revision: u64,
    pub automatic: bool,
    pub cache_idle_seconds: u64,
    pub data_idle_seconds: u64,
    pub minimum_verified_backups: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub artifact_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Protection {
    pub artifact_id: String,
    pub expected_revision: u64,
    pub protected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub artifact_id: String,
    pub expected_revision: u64,
    pub repository_id: Option<String>,
    pub effect: Effect,
    /// An explicit disposal decision; it never overrides a live reference.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegacyRegister {
    pub deployment_id: String,
    pub expected_inventory_revision: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub root_id: String,
    pub repository_id: Option<String>,
    pub label: String,
    pub kind: Kind,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RootSet {
    pub root_id: Option<String>,
    pub repository_id: Option<String>,
    pub expected_revision: u64,
    pub label: String,
    pub path: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Roots {
    pub roots: Vec<Root>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scan {
    pub repository_id: Option<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub include_persistent_data: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    pub artifact_id: String,
    pub revision: u64,
    pub name: String,
    pub kind: Kind,
    pub effect: Effect,
    pub allocated_bytes: Option<u64>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanupPlan {
    pub plan_id: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub items: Vec<PlanItem>,
    pub reclaimable_bytes: u64,
    pub ready: bool,
    pub automatic: bool,
    pub includes_persistent_data: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub plan_id: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobReference {
    pub job_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Cancelling,
    Cancelled,
    Completed,
    Partial,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpaceChange {
    pub filesystem_id: String,
    pub available_before: u64,
    pub available_after: u64,
    pub measured_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemReceipt {
    pub artifact_id: String,
    pub status: String,
    pub code: Option<String>,
    pub measured_bytes_before: Option<u64>,
    pub removed_at_ms: Option<u64>,
    #[serde(default)]
    pub space_change: Option<SpaceChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub job_id: String,
    pub kind: String,
    pub state: JobState,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub actor: String,
    pub plan_id: Option<String>,
    pub receipts: Vec<ItemReceipt>,
    pub reclaimed_bytes: u64,
    /// Items whose removal was verified but whose space change was unavailable.
    #[serde(default)]
    pub unmeasured_items: u32,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub artifact_id: Option<String>,
    pub before_ms: Option<u64>,
    pub limit: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub jobs: Vec<Job>,
    pub next_before_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseSet {
    pub lease_id: Option<String>,
    pub artifact_ids: Vec<String>,
    pub duration_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub lease_id: String,
    pub artifact_ids: Vec<String>,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseRelease {
    pub lease_id: String,
}
