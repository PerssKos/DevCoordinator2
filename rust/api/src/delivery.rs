use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Artifact,
    RegistryPackage,
    LocalExecutable,
    WebDeployment,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deliver {
    pub release_id: String,
    pub path: String,
    pub run_id: String,
    pub check: String,
    pub artifact: String,
    pub manifest_sha256: String,
    pub source_sha256: String,
    pub target: String,
    pub kind: Kind,
    pub verification_file: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Show {
    pub release_id: String,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "crate::review::page_limit")]
    pub limit: u8,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Qualification {
    Qualified,
    PendingExternalEvidence,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub receipt_id: String,
    pub release_id: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub kind: Kind,
    pub target: String,
    pub source_sha256: String,
    pub config_sha256: String,
    pub run_id: String,
    pub check: String,
    pub artifact: String,
    pub artifact_sha256: String,
    pub manifest_sha256: String,
    pub run_metadata_sha256: String,
    pub verification_sha256: Option<String>,
    pub qualification: Qualification,
    pub qualified: bool,
    pub verified_at_ms: Option<u64>,
    pub checked_at_ms: u64,
    pub delivered_at_ms: Option<u64>,
    pub access: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub receipts: Vec<Receipt>,
    pub next_offset: Option<u32>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub version: u8,
    pub kind: Kind,
    pub target: String,
    pub source_sha256: String,
    pub file: String,
    pub observed_sha256: String,
    pub checked_at_ms: u64,
    pub access: String,
    pub observation: VerificationObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<WebDeploymentVerification>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebDeploymentVerification {
    pub deployment_id: String,
    pub generation_number: u32,
    pub http_status: u16,
    pub content_type: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationObservation {
    DownloadMatched,
    RegistryDownloadMatched,
    ExecutableSmokePassed,
    WebRoutePassed,
}
