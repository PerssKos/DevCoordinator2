//! Capability inventories used to prove that a delivery claim is complete.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    Preliminary,
    Complete,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Product,
    TestOnly,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    RealE2e,
    FixtureOnly,
    VisualOnly,
    ExternalBlocked,
    Deferred,
}

/// One requirement or visible capability in a bounded completion inventory.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub scope: Scope,
    pub state: State,
    pub expected_result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// Set this for a rendered product control that users can activate.
    #[serde(default)]
    pub enabled_control: bool,
    /// Runtime journey evidence for an enabled control.
    #[serde(default)]
    pub rendered_evidence_refs: Vec<String>,
}

/// Hash-bound, deliberately small inventory retained beside a governed run.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u8,
    pub claim: Claim,
    pub source_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub capability_id: String,
    pub code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckResult {
    pub valid: bool,
    pub claim: Claim,
    pub source_sha256: String,
    pub capability_count: u32,
    pub real_e2e_count: u32,
    pub incomplete_count: u32,
    pub findings: Vec<Finding>,
}
