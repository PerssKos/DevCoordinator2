//! Explicit, repository-scoped recovery of saved planning history.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bounded, provider-neutral guidance for recovering from a failed operation.
/// This is deliberately separate from the human-oriented error message so CLI,
/// MCP and edge callers can make their own decision without guessing.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Transient,
    Conflict,
    Invalid,
    External,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Wait,
    Inspect,
    Retry,
    Continue,
    Repair,
    Replace,
    AskUser,
    Stop,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOption {
    pub id: String,
    pub action: Action,
    pub operation: Option<String>,
    pub effect: String,
    pub target: Option<String>,
    pub cost: Option<String>,
    pub risk: Option<String>,
    pub prerequisites: Vec<String>,
    pub independent_work_safe: bool,
    pub user_action_required: bool,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guidance {
    pub class: Class,
    pub retryable: bool,
    pub waitable: bool,
    pub safe_to_continue: bool,
    pub state: Option<String>,
    pub reason: String,
    pub run_id: Option<String>,
    pub deployment_id: Option<String>,
    pub generation: Option<u32>,
    pub repository_id: Option<String>,
    pub options: Vec<RecoveryOption>,
    pub field_errors: Vec<FieldError>,
    pub example: Option<serde_json::Value>,
}

impl Guidance {
    pub fn transient(reason: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            class: Class::Transient,
            retryable: false,
            waitable: true,
            safe_to_continue: true,
            state: Some(state.into()),
            reason: reason.into(),
            run_id: None,
            deployment_id: None,
            generation: None,
            repository_id: None,
            options: Vec::new(),
            field_errors: Vec::new(),
            example: None,
        }
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository_id: String,
    pub transaction_dir: String,
    pub backup_sha256: String,
    #[serde(default)]
    pub expected_live_sha256: Option<String>,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub preserve_live_tasks: Vec<PreserveLiveTask>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreserveLiveTask {
    pub task_id: String,
    pub saved_sha256: String,
    pub live_sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub kind: String,
    pub id: String,
    pub original_sequence: i64,
    pub assigned_sequence: i64,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    pub kind: String,
    pub id: String,
    pub fields: Vec<String>,
    pub saved_sha256: String,
    pub live_sha256: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub recovery_id: String,
    pub repository_id: String,
    pub backup_sha256: String,
    pub live_sha256: String,
    pub provenance: String,
    pub status: String,
    pub counts: BTreeMap<String, u64>,
    pub mappings: Vec<Mapping>,
    #[serde(default)]
    pub existing_counts: BTreeMap<String, u64>,
    #[serde(default)]
    pub conflict_count: u64,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub preserved_live_tasks: Vec<PreserveLiveTask>,
}
