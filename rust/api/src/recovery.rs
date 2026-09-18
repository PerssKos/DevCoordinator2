//! Explicit, repository-scoped recovery of saved planning history.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
}
