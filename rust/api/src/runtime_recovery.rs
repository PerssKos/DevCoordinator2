//! Explicit recovery of one deployment's saved ownership metadata.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository_id: String,
    pub deployment_id: String,
    pub transaction_dir: String,
    pub backup_sha256: String,
    #[serde(default)]
    pub expected_live_sha256: Option<String>,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub recovery_id: Option<String>,
    pub repository_id: String,
    pub deployment_id: String,
    pub backup_sha256: String,
    pub live_sha256: String,
    pub provenance: String,
    pub status: String,
    pub current_generation: u32,
    pub saved_generation: u32,
    pub preserved_database_identity: String,
    pub ports: Vec<Port>,
    pub blockers: Vec<String>,
    pub observed_components: std::collections::BTreeMap<String, String>,
    pub database_backup_sha256: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub component: String,
    pub port: u16,
    pub generation: u32,
}
