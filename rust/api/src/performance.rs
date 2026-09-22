//! Read-only Console projections over canonical accounting and review records.
use crate::{
    outcomes::OutcomeMeasurement,
    results::UsageCoverage,
    review::{EvidenceRef, ReviewUsage, Revision},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overview {
    pub repository_id: String,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    #[serde(default)]
    pub outcome_cursor: Option<String>,
    #[serde(default)]
    pub outcome_limit: Option<u32>,
    /// A separate lightweight request keeps lifetime totals from delaying the page.
    #[serde(default)]
    pub totals_only: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reviews {
    pub repository_id: String,
    #[serde(default)]
    pub before: Option<u64>,
    #[serde(default = "crate::review::page_limit")]
    pub limit: u8,
    #[serde(default)]
    pub record_id: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub repository_id: String,
    pub reference: String,
    #[serde(default)]
    pub outcome_cursor: Option<String>,
    #[serde(default)]
    pub outcome_limit: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct OverviewResult {
    pub repository_id: String,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub generated_at_ms: u64,
    pub total_tokens: OutcomeMeasurement,
    pub coverage: UsageCoverage,
    pub usage: Option<ReviewUsage>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub review: Revision,
    pub revision_count: u32,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReviewPage {
    pub records: Vec<ReviewSummary>,
    pub total_reviews: u64,
    pub next_before: Option<u64>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct Evidence {
    pub source: EvidenceRef,
    pub title: String,
    pub body: String,
    pub available: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct MetricChange {
    pub metric: String,
    pub before: OutcomeMeasurement,
    pub after: OutcomeMeasurement,
    pub reduction: Option<f64>,
    pub reduction_percent: Option<f64>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct Comparison {
    pub source: EvidenceRef,
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub verified_improvement: bool,
    pub explanation: String,
    pub metrics: Vec<MetricChange>,
}

#[derive(Clone, Debug, JsonSchema, Serialize, Deserialize)]
pub struct ReviewResult {
    pub review: Revision,
    pub usage: ReviewUsage,
    pub evidence: Vec<Evidence>,
    pub comparisons: Vec<Comparison>,
}
