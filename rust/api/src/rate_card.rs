//! Versioned, administrator-managed API-equivalent token pricing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub include_inactive: bool,
    #[serde(default)]
    pub effective_at_ms: Option<u64>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Set {
    pub expected_revision: u64,
    pub card: RateCard,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    pub card_id: String,
    pub version: u32,
    pub provider: String,
    pub model_pattern: String,
    pub processing_tier: String,
    pub context_tier: String,
    pub effective_from_ms: u64,
    #[serde(default)]
    pub effective_to_ms: Option<u64>,
    pub input_usd_micros_per_million: u64,
    pub cached_input_usd_micros_per_million: u64,
    pub cache_write_usd_micros_per_million: u64,
    pub output_usd_micros_per_million: u64,
    pub source_ref: String,
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_active() -> bool {
    true
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub revision: u64,
    pub cards: Vec<RateCard>,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub revision: u64,
    pub card_id: String,
    pub version: u32,
}
