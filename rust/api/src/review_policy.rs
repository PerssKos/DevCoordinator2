//! Provider-neutral review policy and the versioned reminder delivery contract.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub repository_id: String,
    pub workstream_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Set {
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub review_interval_ms: Option<u64>,
    pub escalation_interval_ms: Option<u64>,
    pub active: bool,
    /// Earlier start retained during migration; never a completion receipt.
    pub window_start_ms: Option<u64>,
    #[serde(default)]
    pub clock_action: Option<ClockAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockAction {
    FollowExisting,
    ResetExisting,
    StartNew,
    RetireExisting,
}

#[derive(Clone, Debug, Eq, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlarmCapability {
    pub alarm_namespace: String,
    pub capability_revision: u8,
    /// Absolute UTC lease expiry in Unix milliseconds.
    pub lease_expires_at: u64,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub owner_thread_id: String,
    pub mode: DeliveryMode,
    pub alarm_namespace: String,
    pub capability_revision: u8,
    /// Absolute UTC lease expiry in Unix milliseconds.
    pub lease_expires_at: u64,
}
#[derive(Clone, Copy, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    CodexAlarm,
}

#[derive(Clone, Debug, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Policy {
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub review_interval_ms: u64,
    pub escalation_interval_ms: u64,
    pub active: bool,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub last_completed_receipt: Option<String>,
    pub due: bool,
    pub escalated: bool,
    pub delivery_route: String,
    pub owner_thread_id: Option<String>,
    /// Absolute UTC lease expiry in Unix milliseconds.
    pub lease_expires_at: Option<u64>,
    pub clock_source: String,
    pub clock_state: String,
    pub clock_start_ms: u64,
    pub clock_due_at_ms: u64,
    pub clock_hard_stop_at_ms: u64,
    pub choice_required: bool,
    pub choice_explanation: Option<String>,
    pub available_actions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reminder {
    pub version: u8,
    pub reminder_id: String,
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub owner_thread_id: String,
    pub alarm_namespace: String,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub due_at_ms: u64,
    pub last_completed_receipt: Option<String>,
    pub escalation: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRequest {
    pub alarm_namespace: String,
    #[serde(default)]
    pub after_id: u64,
}
#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct Pending {
    pub cursor: u64,
    pub reminders: Vec<Reminder>,
    pub next_after_id: Option<u64>,
}
