//! Governed ephemeral test-container protocol types.
//!
//! Container requests intentionally contain no Docker socket, host path, or
//! arbitrary Docker option.  The control daemon derives repository and
//! worktree ownership from the authenticated path and adds the managed labels
//! before invoking Docker.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const fn default_cleanup_status() -> ContainerCleanupStatus {
    ContainerCleanupStatus::Pending
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerCleanupStatus {
    #[serde(rename = "cleanup_pending")]
    #[serde(alias = "pending")]
    #[schemars(rename = "cleanup_pending")]
    Pending,
    #[serde(rename = "cleanup_complete")]
    #[serde(alias = "complete")]
    #[schemars(rename = "cleanup_complete")]
    Complete,
    #[serde(rename = "cleanup_failed")]
    #[serde(alias = "failed")]
    #[schemars(rename = "cleanup_failed")]
    Failed,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerCleanupAttempt {
    pub attempt: u32,
    pub attempted_at: String,
    pub status: ContainerCleanupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default)]
    pub stderr_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default)]
    pub retryable: bool,
}

/// The daemon-owned record for one temporary container.  This is retained in
/// the governed run directory so cleanup can be retried after a daemon
/// restart; callers cannot provide or replace the owner token.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestContainerRecord {
    pub container_id: String,
    pub name: String,
    pub run_id: String,
    pub check: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub owner_token: String,
    pub labels: BTreeMap<String, String>,
    pub image: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub operation_failed: bool,
    pub created_at: String,
    #[serde(default)]
    pub native_execution_began: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver_signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_ref: Option<String>,
    #[serde(default)]
    pub stdout_bytes: u64,
    #[serde(default)]
    pub stderr_bytes: u64,
    #[serde(default = "default_cleanup_status")]
    pub cleanup_status: ContainerCleanupStatus,
    #[serde(default)]
    pub cleanup_attempts: Vec<ContainerCleanupAttempt>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerCreate {
    pub path: String,
    pub run_id: String,
    pub check: String,
    pub image: String,
    #[schemars(length(min = 1, max = 64))]
    pub command: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerReference {
    pub path: String,
    pub run_id: String,
    pub check: String,
    #[schemars(length(equal = 64))]
    pub container_id: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerInspection {
    pub container_id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub native_execution_began: bool,
    pub cleanup_status: ContainerCleanupStatus,
    #[serde(default)]
    pub cleanup_attempts: Vec<ContainerCleanupAttempt>,
    pub retryable: bool,
    pub next_action: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerOperationResult {
    pub operation: String,
    pub record: TestContainerRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<ContainerInspection>,
}

/// Aggregate cleanup receipt used by test finalization.  A run is eligible for
/// a passed terminal status only when `cleanup_status` is `complete`.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerCleanupSummary {
    pub run_id: String,
    pub cleanup_status: ContainerCleanupStatus,
    pub native_execution_started: bool,
    pub cleanup_completed: bool,
    pub pending_count: u32,
    pub failed_count: u32,
    #[serde(default)]
    pub failed_operations: u32,
    pub records: Vec<TestContainerRecord>,
}
