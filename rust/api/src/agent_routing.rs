use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ClientKind;

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    Repository,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    AlwaysSpawn,
    NeverSpawn,
    SkipIfModelMatches,
    SkipIfModelAndEffortMatches,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub role_id: String,
    pub title: String,
    pub position: u16,
    pub retired: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub role_id: String,
    pub action: Action,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub inherited: bool,
    pub stale: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub model: String,
    pub effort: String,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub harness: ClientKind,
    pub models: Vec<String>,
    pub efforts: Vec<String>,
    pub pairs: Vec<Pair>,
    pub reported_at_ms: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub fresh: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub scope: Scope,
    pub repository_id: Option<String>,
    pub harness: ClientKind,
    pub revision: u32,
    pub roles_revision: u32,
    pub roles: Vec<Role>,
    pub rules: Vec<Rule>,
    pub capabilities: Capabilities,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGet {
    pub scope: Scope,
    #[serde(default)]
    pub repository_id: Option<String>,
    pub harness: ClientKind,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSave {
    pub scope: Scope,
    #[serde(default)]
    pub repository_id: Option<String>,
    pub harness: ClientKind,
    pub expected_revision: u32,
    pub rules: Vec<RuleInput>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleInput {
    #[schemars(regex(pattern = r"^[a-z][a-z0-9_]{0,79}$"))]
    pub role_id: String,
    pub action: Action,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesSave {
    pub expected_revision: u32,
    pub roles: Vec<RoleInput>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleInput {
    #[schemars(regex(pattern = r"^[a-z][a-z0-9_]{0,79}$"))]
    pub role_id: String,
    #[schemars(length(min = 1, max = 200))]
    pub title: String,
    pub position: u16,
    pub retired: bool,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesReport {
    pub harness: ClientKind,
    pub models: Vec<String>,
    pub efforts: Vec<String>,
    pub pairs: Vec<Pair>,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionCurrent {
    #[serde(default)]
    pub repository_id: Option<String>,
}
