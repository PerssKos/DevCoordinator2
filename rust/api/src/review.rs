use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::results::{
    UsageActivity, UsageCoverage, UsageSemantics, UsageTime, UsageTools, UsageTotals,
};

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "page_limit")]
    pub limit: u8,
    pub before_decision_seq: Option<u32>,
}

pub fn page_limit() -> u8 {
    10
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub record_id: Option<String>,
    pub expected_revision: u32,
    pub record: ReviewRecord,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Show {
    pub repository_id: String,
    pub record_id: Option<String>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "page_limit")]
    pub limit: u8,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub reference: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewRecord {
    pub version: u8,
    pub repository_id: String,
    pub project_id: String,
    pub workstream_id: Option<String>,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub outcome_id: Option<String>,
    pub experiment: OptimizationExperiment,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OptimizationExperiment {
    pub hypothesis: String,
    pub evidence_refs: Vec<EvidenceRef>,
    pub alternatives: Vec<String>,
    pub chosen_action: String,
    pub baseline: Baseline,
    pub success_criteria: String,
    pub rollback_condition: String,
    pub disposition: Disposition,
    pub result_evidence_refs: Vec<EvidenceRef>,
    pub scope_repo_id: String,
    pub preserves_quality: bool,
    pub reason: String,
    pub observations: Vec<Observation>,
    pub comparison: Option<Comparison>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Comparison {
    pub narrative: String,
    pub input_change_reason: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Baseline {
    pub evidence_refs: Vec<EvidenceRef>,
    pub interpretation: String,
    pub missing_measurements: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Proposed,
    Applied,
    Retained,
    Reverted,
    Inconclusive,
    Unchanged,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceRef {
    pub kind: EvidenceKind,
    pub reference: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Usage,
    Outcome,
    Decision,
    Run,
    Release,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    pub kind: ObservationKind,
    pub interpretation: String,
    pub evidence_refs: Vec<EvidenceRef>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    UserWait,
    IntentionalValidation,
    AvoidableWork,
    Other,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub reference: String,
    pub record_id: String,
    pub revision: u32,
    pub recorded_at_ms: u64,
    pub completed: bool,
    pub record: ReviewRecord,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub records: Vec<Revision>,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<crate::work_context::WorkAttribution>,
    pub source: EvidenceRef,
    pub source_sha256: Option<String>,
    pub config_sha256: Option<String>,
    pub title: String,
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub repository_id: String,
    pub workstream_id: Option<String>,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub generated_at_ms: u64,
    pub source_refs: Vec<EvidenceRef>,
    pub coverage_gaps: Vec<String>,
    pub usage: ReviewUsage,
    pub evidence: Vec<Evidence>,
    pub next_offset: Option<u32>,
    pub standing_decisions: Vec<Evidence>,
    pub next_before_decision_seq: Option<u32>,
    pub interpretation_rules: Vec<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewUsage {
    pub coverage: UsageCoverage,
    pub totals: UsageTotals,
    pub activities: Vec<UsageActivity>,
    pub time: UsageTime,
    pub tools: UsageTools,
    pub semantics: UsageSemantics,
}
