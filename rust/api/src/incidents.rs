//! Observations and explicit human-facing incident dispositions.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatus {
    #[default]
    Unreviewed,
    Handling,
    Suppressed,
    Escalated,
    Dismissed,
    Resolved,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentView {
    #[default]
    Attention,
    Dismissed,
    All,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IncidentCategory {
    #[default]
    Operational,
    Development,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentListParams {
    #[serde(default)]
    pub view: IncidentView,
    #[serde(default = "page_size")]
    pub limit: u16,
    #[serde(default)]
    pub before: Option<String>,
}
fn page_size() -> u16 {
    20
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Incident {
    #[serde(default)]
    pub category: IncidentCategory,
    pub incident_id: String,
    pub alert_key: String,
    pub opened_at: String,
    pub last_seen_at: String,
    pub repository_id: Option<String>,
    pub repository_name: Option<String>,
    pub deployment_id: Option<String>,
    pub deployment_name: Option<String>,
    pub component: Option<String>,
    pub severity: String,
    pub status: IncidentStatus,
    pub condition_active: bool,
    pub summary: String,
    pub what_happened: String,
    pub agent_response: Option<String>,
    pub escalation_reason: Option<String>,
    pub next_step: Option<String>,
    pub revision: u32,
    pub updated_at: Option<String>,
    pub updated_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentList {
    pub incidents: Vec<Incident>,
    pub attention_count: usize,
    pub dismissed_count: usize,
    pub total: usize,
    pub next_before: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IncidentUpdate {
    #[serde(default)]
    pub category: Option<IncidentCategory>,
    pub incident_id: String,
    pub expected_revision: u32,
    pub status: IncidentStatus,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub what_happened: Option<String>,
    #[serde(default)]
    pub agent_response: Option<String>,
    #[serde(default)]
    pub escalation_reason: Option<String>,
    #[serde(default)]
    pub next_step: Option<String>,
}
