//! Typed local-control protocol shared by the daemon, CLI, MCP adapter, and edge.

use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

pub mod configuration;
pub mod delivery;
pub mod glossary;
pub mod params;
pub mod results;
pub mod review;
pub mod work_context;

pub const PROTOCOL_VERSION: u8 = 2;
pub const DATABASE_SCHEMA_VERSION: u32 = 20;
pub const MAX_REQUEST_BYTES: usize = 65_536;
pub const MAX_RESPONSE_BYTES: usize = 262_144;
pub const MAX_ERROR_DETAIL_BYTES: usize = 4_096;
pub const READ_TIMEOUT_SECONDS: u64 = 5;
pub const WRITE_TIMEOUT_SECONDS: u64 = 10;

#[derive(Clone, Copy, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Codex,
    Claude,
    Cursor,
    Antigravity,
    Human,
    Edge,
    #[default]
    Other,
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientContext {
    #[serde(default)]
    pub kind: ClientKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<work_context::WorkContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_source: Option<work_context::WorkSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_diagnostic: Option<work_context::WorkDiagnostic>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub protocol: u8,
    pub id: String,
    pub operation: String,
    #[serde(default = "empty_object")]
    pub params: Value,
    #[serde(default)]
    pub client: ClientContext,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolInvalid,
    ProtocolUnsupported,
    RequestTooLarge,
    OperationUnknown,
    ParamsInvalid,
    RepositoryNotFound,
    RepositoryArchived,
    RepositoryArchiveBlocked,
    RepositoryConfigInvalid,
    AuthorizationRequired,
    ConfigurationConflict,
    ConfigurationInvalid,
    ConfigurationRestartRequired,
    WorktreeBusy,
    TestNotFound,
    TestLogUnavailable,
    LogNotFound,
    LogExpired,
    CursorStale,
    StructuredEvidenceInvalid,
    TestEvidenceExpired,
    TestEvidenceNotFound,
    TestEvidenceTampered,
    TestArtifactExpired,
    TestArtifactNotFound,
    TestArtifactTampered,
    TestStartFailed,
    TestsDraining,
    UnitStopFailed,
    DeploymentNotFound,
    Busy,
    DeploymentApplyFailed,
    DeploymentActionFailed,
    ObservedOnly,
    RollbackUnavailable,
    PermissionDenied,
    UserNotFound,
    TaskNotFound,
    ReleaseNotFound,
    DecisionNotFound,
    GlossaryNotFound,
    GlossaryConflict,
    InternalError,
    DaemonUnavailable,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = serde_json::to_value(self).map_err(|_| fmt::Error)?;
        f.write_str(value.as_str().ok_or(fmt::Error)?)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseEnvelope {
    Success {
        protocol: u8,
        id: String,
        ok: True,
        data: Value,
    },
    Failure {
        protocol: u8,
        id: String,
        ok: False,
        error: ErrorBody,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct True;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct False;

impl Serialize for True {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for True {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected true"))
        }
    }
}

impl Serialize for False {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(false)
    }
}

impl<'de> Deserialize<'de> for False {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Err(serde::de::Error::custom("expected false"))
        } else {
            Ok(Self)
        }
    }
}

impl JsonSchema for True {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "true".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        serde_json::json!({"const": true})
            .try_into()
            .expect("valid schema")
    }
}

impl JsonSchema for False {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "false".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        serde_json::json!({"const": false})
            .try_into()
            .expect("valid schema")
    }
}

impl ResponseEnvelope {
    pub fn success(id: impl Into<String>, data: impl Serialize) -> Result<Self, ProtocolError> {
        Ok(Self::Success {
            protocol: PROTOCOL_VERSION,
            id: id.into(),
            ok: True,
            data: serde_json::to_value(data).map_err(ProtocolError::serialization)?,
        })
    }

    pub fn failure(id: impl Into<String>, error: ProtocolError) -> Self {
        Self::Failure {
            protocol: PROTOCOL_VERSION,
            id: id.into(),
            ok: False,
            error: error.into_body(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub detail: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: String::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = truncate_utf8(&detail.into(), MAX_ERROR_DETAIL_BYTES);
        self
    }

    fn serialization(error: serde_json::Error) -> Self {
        Self::new(ErrorCode::InternalError, "could not serialize response")
            .with_detail(error.to_string())
    }

    fn into_body(self) -> ErrorBody {
        ErrorBody {
            code: self.code,
            message: self.message,
            detail: truncate_utf8(&self.detail, MAX_ERROR_DETAIL_BYTES),
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Public,
    #[serde(rename = "self")]
    Self_,
    Server,
    Repository,
    Deployment,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Anonymous,
    #[serde(rename = "self")]
    Self_,
    Viewer,
    Operator,
    Administrator,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    Append,
    Reversible,
    Destructive,
    External,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
pub struct OperationPolicy {
    pub scope: Scope,
    pub role: Role,
    pub effect: Effect,
    pub idempotent: bool,
}

impl OperationPolicy {
    pub const fn new(scope: Scope, role: Role, effect: Effect, idempotent: bool) -> Self {
        Self {
            scope,
            role,
            effect,
            idempotent,
        }
    }

    pub const fn read_only(self) -> bool {
        matches!(self.effect, Effect::Read)
    }

    pub const fn destructive(self) -> bool {
        matches!(self.effect, Effect::Destructive)
    }

    pub const fn open_world(self) -> bool {
        matches!(self.effect, Effect::External)
    }
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PingData {
    pub daemon_version: String,
    pub protocol_version: u8,
    pub schema_version: u32,
    pub executor_schema: u8,
    pub source_commit: String,
    pub socket: String,
}

#[derive(Clone, Copy, Debug)]
pub struct OperationDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub policy: OperationPolicy,
    pub cli_routes: &'static [CliRouteDefinition],
    pub cli_exclusion: Option<&'static str>,
    pub mcp_names: &'static [&'static str],
    pub mcp_exclusion: Option<&'static str>,
    pub mcp_overrides: &'static [McpToolOverride],
    pub input_schema: fn() -> Value,
    pub output_schema: fn() -> Value,
    pub validate_params: fn(&Value) -> Result<(), ProtocolError>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliDispatch {
    Protocol,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliRouteDefinition {
    pub route: &'static str,
    pub dispatch: CliDispatch,
}

#[derive(Clone, Copy, Debug)]
pub struct McpToolOverride {
    pub name: &'static str,
    pub input_schema: fn() -> Value,
    pub validate_params: fn(&Value) -> Result<(), ProtocolError>,
    pub transform: McpArgumentTransform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpArgumentTransform {
    Identity,
    ClearCapacity,
}

impl McpArgumentTransform {
    pub fn apply(self, params: Value) -> Value {
        match self {
            Self::Identity => params,
            Self::ClearCapacity => serde_json::json!({"cap": null}),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct McpToolDefinition {
    pub name: &'static str,
    pub operation: &'static OperationDefinition,
    pub input_schema: fn() -> Value,
    pub validate_params: fn(&Value) -> Result<(), ProtocolError>,
    pub transform: McpArgumentTransform,
}

fn schema_for<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema is serializable")
}

fn validate_params<T>(value: &Value) -> Result<(), ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value::<T>(value.clone())
        .map(|_| ())
        .map_err(|error| {
            ProtocolError::new(ErrorCode::ParamsInvalid, "operation parameters are invalid")
                .with_detail(error.to_string())
        })
}

const READ_PUBLIC: OperationPolicy =
    OperationPolicy::new(Scope::Public, Role::Anonymous, Effect::Read, true);
const READ_SERVER_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Server, Role::Administrator, Effect::Read, true);
const READ_DEPLOYMENT_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Deployment, Role::Administrator, Effect::Read, true);
const APPEND_SERVER_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Server, Role::Administrator, Effect::Append, false);
const IDEMPOTENT_APPEND_SERVER_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Server, Role::Administrator, Effect::Append, true);
const REVERSIBLE_SERVER_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Server, Role::Administrator, Effect::Reversible, true);
const DESTRUCTIVE_SERVER_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Server,
    Role::Administrator,
    Effect::Destructive,
    true,
);
const READ_REPOSITORY_ADMIN: OperationPolicy =
    OperationPolicy::new(Scope::Repository, Role::Administrator, Effect::Read, true);
const APPEND_REPOSITORY_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Repository,
    Role::Administrator,
    Effect::Append,
    false,
);
const REVERSIBLE_REPOSITORY_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Repository,
    Role::Administrator,
    Effect::Reversible,
    false,
);
const IDEMPOTENT_REVERSIBLE_REPOSITORY_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Repository,
    Role::Administrator,
    Effect::Reversible,
    true,
);
const DESTRUCTIVE_REPOSITORY_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Repository,
    Role::Administrator,
    Effect::Destructive,
    false,
);
const IDEMPOTENT_DESTRUCTIVE_REPOSITORY_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Repository,
    Role::Administrator,
    Effect::Destructive,
    true,
);
const READ_REPOSITORY_VIEWER: OperationPolicy =
    OperationPolicy::new(Scope::Repository, Role::Viewer, Effect::Read, true);
const READ_REPOSITORY_OPERATOR: OperationPolicy =
    OperationPolicy::new(Scope::Repository, Role::Operator, Effect::Read, true);
const READ_DEPLOYMENT_VIEWER: OperationPolicy =
    OperationPolicy::new(Scope::Deployment, Role::Viewer, Effect::Read, true);
const REVERSIBLE_DEPLOYMENT_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Deployment,
    Role::Administrator,
    Effect::Reversible,
    false,
);
const IDEMPOTENT_REVERSIBLE_DEPLOYMENT_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Deployment,
    Role::Administrator,
    Effect::Reversible,
    true,
);
const REVERSIBLE_DEPLOYMENT_OPERATOR: OperationPolicy =
    OperationPolicy::new(Scope::Deployment, Role::Operator, Effect::Reversible, false);
const IDEMPOTENT_REVERSIBLE_DEPLOYMENT_OPERATOR: OperationPolicy =
    OperationPolicy::new(Scope::Deployment, Role::Operator, Effect::Reversible, true);
const DESTRUCTIVE_DEPLOYMENT_ADMIN: OperationPolicy = OperationPolicy::new(
    Scope::Deployment,
    Role::Administrator,
    Effect::Destructive,
    true,
);
const READ_SELF: OperationPolicy =
    OperationPolicy::new(Scope::Self_, Role::Self_, Effect::Read, true);
const APPEND_SELF: OperationPolicy =
    OperationPolicy::new(Scope::Self_, Role::Self_, Effect::Append, false);
const DESTRUCTIVE_SELF: OperationPolicy =
    OperationPolicy::new(Scope::Self_, Role::Self_, Effect::Destructive, true);
const EXTERNAL_SELF: OperationPolicy =
    OperationPolicy::new(Scope::Self_, Role::Self_, Effect::External, false);
const IDEMPOTENT_EXTERNAL_SELF: OperationPolicy =
    OperationPolicy::new(Scope::Self_, Role::Self_, Effect::External, true);

macro_rules! operation {
    ($name:literal, $description:literal, $policy:expr, $dispatch:ident [$($cli:literal),+ $(,)?], [$($mcp:literal),* $(,)?], $input:ty, $output:ty) => {
        OperationDefinition {
            name: $name,
            description: $description,
            policy: $policy,
            cli_routes: &[$(CliRouteDefinition {
                route: $cli,
                dispatch: CliDispatch::$dispatch,
            }),+],
            cli_exclusion: None,
            mcp_names: &[$($mcp),*],
            mcp_exclusion: mcp_exclusion!($($mcp),*),
            mcp_overrides: &[],
            input_schema: schema_for::<$input>,
            output_schema: schema_for::<$output>,
            validate_params: validate_params::<$input>,
        }
    };
    ($name:literal, $description:literal, $policy:expr, excluded $reason:literal, [$($mcp:literal),* $(,)?], $input:ty, $output:ty) => {
        OperationDefinition {
            name: $name,
            description: $description,
            policy: $policy,
            cli_routes: &[],
            cli_exclusion: Some($reason),
            mcp_names: &[$($mcp),*],
            mcp_exclusion: mcp_exclusion!($($mcp),*),
            mcp_overrides: &[],
            input_schema: schema_for::<$input>,
            output_schema: schema_for::<$output>,
            validate_params: validate_params::<$input>,
        }
    };
}

macro_rules! mcp_exclusion {
    () => {
        Some("Deliberately not exposed as an MCP tool.")
    };
    ($($mcp:literal),+ $(,)?) => {
        None
    };
}

pub static OPERATIONS: &[OperationDefinition] = &[
    operation!(
        "ping",
        "Report daemon, schema, protocol, source, and socket identity.",
        READ_PUBLIC,
        Protocol["ping"],
        [],
        EmptyParams,
        PingData
    ),
    operation!(
        "user.whoami",
        "Show the current public identity and grants.",
        READ_PUBLIC,
        excluded "The signed-in identity is served directly by the Console edge.",
        [],
        params::Empty,
        results::WhoAmI
    ),
    operation!(
        "user.accept_invitation",
        "Accept an invitation for the edge-authenticated identity.",
        APPEND_SELF,
        excluded "Invitation acceptance is bound to the authenticated Console edge.",
        [],
        params::AcceptInvitation,
        results::AcceptedInvitation
    ),
    operation!(
        "user.list",
        "List public Console users and invitations.",
        READ_SERVER_ADMIN,
        excluded "User administration is a Console-only owner workflow.",
        [],
        params::Empty,
        results::UserList
    ),
    operation!(
        "user.invite",
        "Invite a public Console user.",
        APPEND_SERVER_ADMIN,
        excluded "User administration is a Console-only owner workflow.",
        [],
        params::InviteUser,
        results::InvitedUser
    ),
    operation!(
        "user.remove",
        "Remove one public Console user.",
        DESTRUCTIVE_SERVER_ADMIN,
        excluded "User administration is a Console-only owner workflow.",
        [],
        params::EmailOnly,
        results::RemovedUser
    ),
    operation!(
        "grant.set",
        "Set one deployment access role.",
        IDEMPOTENT_REVERSIBLE_DEPLOYMENT_ADMIN,
        excluded "Access grants are a Console-only owner workflow.",
        [],
        params::SetGrant,
        results::GrantSet
    ),
    operation!(
        "grant.remove",
        "Remove one deployment access grant.",
        DESTRUCTIVE_DEPLOYMENT_ADMIN,
        excluded "Access grants are a Console-only owner workflow.",
        [],
        params::RemoveGrant,
        results::GrantRemoved
    ),
    operation!(
        "repository.register",
        "Register the repository containing a path.",
        IDEMPOTENT_APPEND_SERVER_ADMIN,
        Protocol["repository register"],
        [],
        params::PathOnly,
        results::RegisteredRepository
    ),
    operation!(
        "repository.list",
        "List registered repositories.",
        READ_SERVER_ADMIN,
        Protocol["repository list"],
        ["repository_list"],
        params::RepositoryList,
        results::RepositoryList
    ),
    operation!(
        "repository.status",
        "Show one registered repository.",
        READ_REPOSITORY_ADMIN,
        Protocol["repository status"],
        [],
        params::PathOnly,
        results::RepositoryStatus
    ),
    operation!(
        "repository.presentation.update",
        "Choose a repository's Console name and icon without changing its identity.",
        IDEMPOTENT_REVERSIBLE_REPOSITORY_ADMIN,
        excluded "Repository appearance is a Console-only administrator workflow.",
        [],
        params::RepositoryPresentationUpdate,
        results::RepositoryPresentation
    ),
    operation!(
        "repository.archive",
        "Archive a repository after its blockers are cleared.",
        REVERSIBLE_SERVER_ADMIN,
        Protocol["repository archive"],
        ["repository_archive"],
        params::ArchiveRepository,
        results::Repository
    ),
    operation!(
        "repository.unarchive",
        "Restore one archived repository.",
        REVERSIBLE_SERVER_ADMIN,
        Protocol["repository unarchive"],
        ["repository_unarchive"],
        params::UnarchiveRepository,
        results::Repository
    ),
    operation!(
        "test.start",
        "Start or supersede one governed validation run.",
        DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["test start"],
        ["test_start"],
        params::StartTest,
        results::TestStarted
    ),
    operation!(
        "test.retry",
        "Retry one failed check from a completed run.",
        DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["test retry"],
        ["test_retry"],
        params::RetryTest,
        results::TestStarted
    ),
    operation!(
        "test.status",
        "Show the current governed validation result.",
        READ_REPOSITORY_ADMIN,
        Protocol["test status"],
        ["test_status"],
        params::PathOnly,
        results::TestSummary
    ),
    operation!(
        "test.log.catalog",
        "List retained log metadata without content.",
        READ_REPOSITORY_ADMIN,
        Protocol["test log catalog"],
        ["test_log_catalog"],
        params::LogCatalog,
        results::LogCatalog
    ),
    operation!(
        "test.log.tail",
        "Read a bounded final-line log slice.",
        READ_REPOSITORY_ADMIN,
        Protocol["test log tail"],
        ["test_log_tail"],
        params::LogTail,
        results::LogContent
    ),
    operation!(
        "test.log.search",
        "Search one retained stream literally.",
        READ_REPOSITORY_ADMIN,
        Protocol["test log search"],
        ["test_log_search"],
        params::LogSearch,
        results::LogSearch
    ),
    operation!(
        "test.log.range",
        "Read one exact bounded log range.",
        READ_REPOSITORY_ADMIN,
        Protocol["test log range"],
        ["test_log_range"],
        params::LogRange,
        results::LogContent
    ),
    operation!(
        "test.log.failure_context",
        "Read deterministic bounded failure context.",
        READ_REPOSITORY_ADMIN,
        Protocol["test log failure-context"],
        ["test_log_failure_context"],
        params::LogFailureContext,
        results::LogFailureContext
    ),
    operation!(
        "test.log.retention.get",
        "Show governed-log retention settings.",
        READ_SERVER_ADMIN,
        Protocol["test log retention show"],
        ["test_log_retention_show"],
        params::Empty,
        results::Retention
    ),
    operation!(
        "test.log.retention.set",
        "Change governed-log retention settings.",
        DESTRUCTIVE_SERVER_ADMIN,
        Protocol["test log retention set"],
        ["test_log_retention_set"],
        params::SetRetention,
        results::Retention
    ),
    operation!(
        "test.evidence.get",
        "List retained journey evidence.",
        READ_REPOSITORY_ADMIN,
        Protocol["test evidence show"],
        ["test_evidence_get"],
        params::EvidenceReference,
        results::EvidenceGet
    ),
    operation!(
        "test.evidence.image",
        "Read one verified screenshot chunk.",
        READ_REPOSITORY_ADMIN,
        Protocol["test evidence image"],
        ["test_evidence_image"],
        params::EvidenceImage,
        results::ImageChunk
    ),
    operation!(
        "test.artifact.catalog",
        "List retained artifact-tree metadata.",
        READ_REPOSITORY_ADMIN,
        Protocol["test artifact catalog"],
        ["test_artifact_catalog"],
        params::ArtifactCatalog,
        results::ArtifactCatalog
    ),
    operation!(
        "test.artifact.file",
        "Read one verified retained artifact chunk.",
        READ_REPOSITORY_ADMIN,
        Protocol["test artifact file"],
        ["test_artifact_file"],
        params::ArtifactFile,
        results::ArtifactChunk
    ),
    operation!(
        "test.evidence.feedback.create",
        "Create screenshot-anchored project feedback.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["test evidence feedback create"],
        ["test_evidence_feedback_create"],
        params::CreateFeedback,
        results::FeedbackCreated
    ),
    operation!(
        "test.evidence.lookup",
        "Open an exact retained screenshot run without relying on the latest-runs list.",
        READ_SERVER_ADMIN,
        Protocol["test evidence lookup"],
        ["test_evidence_lookup"],
        params::EvidenceLookup,
        results::EvidenceLookup
    ),
    operation!(
        "test.evidence.feedback.reply",
        "Reply to screenshot-anchored feedback.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["test evidence feedback reply"],
        ["test_evidence_feedback_reply"],
        params::FeedbackReply,
        results::FeedbackMutation
    ),
    operation!(
        "test.evidence.feedback.edit",
        "Edit the caller's feedback comment.",
        REVERSIBLE_REPOSITORY_ADMIN,
        Protocol["test evidence feedback edit"],
        ["test_evidence_feedback_edit"],
        params::FeedbackEdit,
        results::FeedbackMutation
    ),
    operation!(
        "test.evidence.feedback.state",
        "Resolve or reopen screenshot feedback.",
        IDEMPOTENT_REVERSIBLE_REPOSITORY_ADMIN,
        Protocol["test evidence feedback state"],
        ["test_evidence_feedback_state"],
        params::FeedbackStateChange,
        results::FeedbackMutation
    ),
    operation!(
        "test.evidence.feedback.delete",
        "Delete the caller's screenshot annotation.",
        IDEMPOTENT_DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["test evidence feedback delete"],
        ["test_evidence_feedback_delete"],
        params::FeedbackDelete,
        results::FeedbackMutation
    ),
    operation!(
        "test.stop",
        "Cancel the current governed run.",
        IDEMPOTENT_DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["test stop"],
        ["test_stop"],
        params::StopTest,
        results::StopTest
    ),
    operation!(
        "test.history",
        "List bounded run history without logs or file contents.",
        READ_REPOSITORY_ADMIN,
        Protocol["test history"],
        ["test_history"],
        params::TestHistory,
        results::TestHistory
    ),
    operation!(
        "test.list",
        "List current governed runs.",
        READ_SERVER_ADMIN,
        Protocol["test list"],
        ["test_list"],
        params::TestList,
        results::TestList
    ),
    operation!(
        "test.capacity.get",
        "Show host-wide validation capacity.",
        READ_SERVER_ADMIN,
        Protocol["test capacity show"],
        ["test_capacity_show"],
        params::Empty,
        results::Capacity
    ),
    OperationDefinition {
        name: "test.capacity.set",
        description: "Set or clear the validation capacity cap.",
        policy: REVERSIBLE_SERVER_ADMIN,
        cli_routes: &[
            CliRouteDefinition {
                route: "test capacity set",
                dispatch: CliDispatch::Protocol,
            },
            CliRouteDefinition {
                route: "test capacity clear",
                dispatch: CliDispatch::Protocol,
            },
        ],
        cli_exclusion: None,
        mcp_names: &["test_capacity_set"],
        mcp_exclusion: None,
        mcp_overrides: &[McpToolOverride {
            name: "test_capacity_clear",
            input_schema: schema_for::<params::Empty>,
            validate_params: validate_params::<params::Empty>,
            transform: McpArgumentTransform::ClearCapacity,
        }],
        input_schema: schema_for::<params::SetCapacity>,
        output_schema: schema_for::<results::Capacity>,
        validate_params: validate_params::<params::SetCapacity>,
    },
    operation!(
        "config.get",
        "Inspect live configuration revisions and bounded change history without exposing private settings.",
        READ_SERVER_ADMIN,
        Protocol["config show"],
        ["config_get"],
        EmptyParams,
        configuration::Snapshot
    ),
    operation!(
        "config.env.set",
        "Grant or revoke one declared repository environment-file authorization and activate it without restart.",
        REVERSIBLE_SERVER_ADMIN,
        Protocol["config authorize", "config revoke"],
        ["config_env_set"],
        params::SetComposeEnvAuthorization,
        configuration::Snapshot
    ),
    operation!(
        "config.reload",
        "Validate and activate reloadable private settings, retaining the last working state on failure.",
        REVERSIBLE_SERVER_ADMIN,
        Protocol["config reload"],
        ["config_reload"],
        configuration::Revision,
        configuration::Snapshot
    ),
    operation!(
        "deployment.preflight",
        "Check declared deployment prerequisites without registering or changing runtime resources.",
        READ_DEPLOYMENT_ADMIN,
        Protocol["deployment preflight"],
        ["deployment_preflight"],
        params::DeploymentReference,
        results::DeploymentPreflight
    ),
    operation!(
        "deployment.list",
        "List visible deployments.",
        READ_DEPLOYMENT_VIEWER,
        Protocol["deployment list"],
        ["deployment_list"],
        params::DeploymentList,
        results::DeploymentList
    ),
    operation!(
        "deployment.status",
        "Show one deployment.",
        READ_DEPLOYMENT_VIEWER,
        Protocol["deployment status"],
        ["deployment_status"],
        params::DeploymentReference,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.logs",
        "Read bounded deployment logs.",
        READ_DEPLOYMENT_VIEWER,
        Protocol["deployment logs"],
        ["deployment_logs"],
        params::DeploymentLogs,
        results::DeploymentLog
    ),
    operation!(
        "deployment.apply",
        "Apply a declared deployment.",
        REVERSIBLE_DEPLOYMENT_ADMIN,
        Protocol["deployment apply"],
        ["deployment_apply"],
        params::DeploymentReference,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.rollback",
        "Roll back to the prior generation.",
        REVERSIBLE_DEPLOYMENT_ADMIN,
        Protocol["deployment rollback"],
        ["deployment_rollback"],
        params::DeploymentReference,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.start",
        "Start a deployment or selected service.",
        IDEMPOTENT_REVERSIBLE_DEPLOYMENT_OPERATOR,
        Protocol["deployment start"],
        ["deployment_start"],
        params::DeploymentControl,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.stop",
        "Stop a deployment or selected service.",
        IDEMPOTENT_REVERSIBLE_DEPLOYMENT_OPERATOR,
        Protocol["deployment stop"],
        ["deployment_stop"],
        params::DeploymentControl,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.restart",
        "Restart a deployment or selected service.",
        REVERSIBLE_DEPLOYMENT_OPERATOR,
        Protocol["deployment restart"],
        ["deployment_restart"],
        params::DeploymentControl,
        results::DeploymentStatus
    ),
    operation!(
        "deployment.set_domain",
        "Set or clear a deployment route.",
        IDEMPOTENT_REVERSIBLE_DEPLOYMENT_ADMIN,
        Protocol["deployment set-domain"],
        ["deployment_set_domain"],
        params::SetDomain,
        results::DomainChanged
    ),
    operation!(
        "deployment.remove",
        "Remove a deployment with an explicit data outcome.",
        DESTRUCTIVE_DEPLOYMENT_ADMIN,
        Protocol["deployment remove"],
        [],
        params::RemoveDeployment,
        results::DeploymentRemoved
    ),
    operation!(
        "health.summary",
        "Show host health and active alerts.",
        READ_SERVER_ADMIN,
        Protocol["health summary"],
        ["health_summary"],
        params::Empty,
        results::HealthSummary
    ),
    operation!(
        "health.repositories",
        "Show visible per-repository resource use.",
        READ_REPOSITORY_VIEWER,
        Protocol["health repositories"],
        ["health_repositories"],
        params::Empty,
        results::HealthRepositories
    ),
    operation!(
        "health.repository",
        "Show one repository's resource use.",
        READ_REPOSITORY_VIEWER,
        Protocol["health repository"],
        ["health_repository"],
        params::PathOnly,
        results::HealthRepository
    ),
    operation!(
        "health.history",
        "Show bounded resource history.",
        READ_REPOSITORY_VIEWER,
        Protocol["health history"],
        [],
        params::HealthHistory,
        results::HealthHistory
    ),
    operation!(
        "health.containers",
        "List every container with truthful attribution.",
        READ_SERVER_ADMIN,
        Protocol["health containers"],
        ["health_containers"],
        params::Empty,
        results::ContainerList
    ),
    operation!(
        "health.container_remove",
        "Remove one exact unmanaged container.",
        DESTRUCTIVE_SERVER_ADMIN,
        excluded "Container removal is deliberately exposed only by the authenticated Console.",
        [],
        params::RemoveContainer,
        results::RemovedContainer
    ),
    operation!(
        "glossary.list",
        "Browse shared or effective project terminology; never application messages.",
        READ_REPOSITORY_VIEWER,
        Protocol["glossary list"],
        ["glossary_list"],
        glossary::List,
        glossary::Page
    ),
    operation!(
        "glossary.resolve",
        "Read the effective glossary before UI work, with pinned shared revision and provenance. Follow the project's own localization mechanism; paginate all needed concepts.",
        READ_REPOSITORY_VIEWER,
        Protocol["glossary resolve"],
        ["glossary_resolve"],
        glossary::List,
        glossary::Page
    ),
    operation!(
        "glossary.get",
        "Read one multilingual concept, optionally at a historical scope revision.",
        READ_REPOSITORY_VIEWER,
        Protocol["glossary get"],
        ["glossary_get"],
        glossary::Get,
        glossary::Detail
    ),
    operation!(
        "glossary.save",
        "Create or revise a concept using an exact expected scope revision. Mandatory inherited concepts cannot be overridden.",
        REVERSIBLE_REPOSITORY_ADMIN,
        Protocol["glossary save"],
        ["glossary_save"],
        glossary::Save,
        glossary::Mutation
    ),
    operation!(
        "glossary.configure",
        "Revise glossary languages and guidance or explicitly adopt a shared revision; this does not change localization files.",
        REVERSIBLE_REPOSITORY_ADMIN,
        Protocol["glossary configure"],
        ["glossary_configure"],
        glossary::Configure,
        glossary::Mutation
    ),
    operation!(
        "glossary.inherit",
        "Remove a project specialization and return to its pinned shared concept, preserving history.",
        REVERSIBLE_REPOSITORY_ADMIN,
        Protocol["glossary inherit"],
        ["glossary_inherit"],
        glossary::Inherit,
        glossary::Mutation
    ),
    operation!(
        "glossary.history",
        "Read bounded permanent glossary revision history.",
        READ_REPOSITORY_VIEWER,
        Protocol["glossary history"],
        ["glossary_history"],
        glossary::History,
        glossary::HistoryPage
    ),
    operation!(
        "glossary.check",
        "Check explicitly identified concept usages against approved localized terms. This is not a general prose or translation-quality detector.",
        READ_REPOSITORY_VIEWER,
        Protocol["glossary check"],
        ["glossary_check"],
        glossary::Check,
        glossary::CheckResult
    ),
    operation!(
        "glossary.impact",
        "Inspect which projects have adopted shared glossary revisions.",
        READ_SERVER_ADMIN,
        Protocol["glossary impact"],
        ["glossary_impact"],
        glossary::ImpactRequest,
        glossary::Impact
    ),
    operation!(
        "plan.overview",
        "Show releases and the active completion plan.",
        READ_REPOSITORY_VIEWER,
        Protocol["plan overview"],
        ["plan_overview"],
        params::PlanReference,
        results::PlanOverview
    ),
    operation!(
        "task.search",
        "Search all task states, including dropped history, without changing the active plan.",
        READ_REPOSITORY_VIEWER,
        Protocol["task search"],
        ["task_search"],
        params::TaskSearch,
        results::TaskSearch
    ),
    operation!(
        "task.history",
        "Show one task and its permanent history.",
        READ_REPOSITORY_VIEWER,
        Protocol["task history"],
        ["task_history"],
        params::TaskHistory,
        results::TaskHistory
    ),
    operation!(
        "task.create",
        "Create one completion-ledger task.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["task create"],
        ["task_create"],
        params::TaskCreate,
        results::TaskCreated
    ),
    operation!(
        "task.update",
        "Append a task state or wording change.",
        DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["task update"],
        ["task_update"],
        params::TaskUpdate,
        results::TaskMutation
    ),
    operation!(
        "release.create",
        "Create a planned release.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["release create"],
        ["release_create"],
        params::ReleaseCreate,
        results::Release
    ),
    operation!(
        "release.update",
        "Change a planned release.",
        DESTRUCTIVE_REPOSITORY_ADMIN,
        Protocol["release update"],
        [],
        params::ReleaseUpdate,
        results::Release
    ),
    operation!(
        "release.request",
        "Request a preview deployment.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["release request"],
        [],
        params::ReleaseRequest,
        results::Release
    ),
    operation!(
        "release.deliver",
        "Attach real delivery evidence to a release.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["release deliver"],
        ["release_deliver"],
        params::ReleaseDeliver,
        results::ReleaseDelivered
    ),
    operation!(
        "decision.tail",
        "Read the rolling decision summary and latest decisions.",
        READ_REPOSITORY_VIEWER,
        Protocol["decision tail"],
        ["decision_tail"],
        params::DecisionTail,
        results::DecisionTail
    ),
    operation!(
        "review.prepare",
        "Prepare bounded canonical usage and outcome evidence without scheduling work.",
        READ_REPOSITORY_ADMIN,
        Protocol["review prepare"],
        ["review_prepare"],
        review::Prepare,
        review::Prepared
    ),
    operation!(
        "review.record",
        "Append an evidence-backed review revision with optimistic concurrency.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["review record"],
        ["review_record"],
        review::Record,
        review::Revision
    ),
    operation!(
        "review.show",
        "Read bounded immutable review revisions.",
        READ_REPOSITORY_ADMIN,
        Protocol["review list"],
        ["review_show"],
        review::Show,
        review::Page
    ),
    operation!(
        "review.receipt",
        "Read one exact immutable structured review revision.",
        READ_REPOSITORY_ADMIN,
        Protocol["review show"],
        ["review_receipt"],
        review::Reference,
        review::Revision
    ),
    operation!(
        "release.evidence",
        "Read an exact delivery receipt; qualification and verification time are server-derived.",
        READ_REPOSITORY_ADMIN,
        Protocol["release evidence"],
        ["release_evidence"],
        review::Reference,
        delivery::Receipt
    ),
    operation!(
        "release.deliver_evidence",
        "Attach retained delivery proof or explicitly pending external evidence.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["release deliver-evidence"],
        ["release_deliver_evidence"],
        delivery::Deliver,
        delivery::Receipt
    ),
    operation!(
        "release.evidence_show",
        "Read bounded immutable delivery evidence receipts.",
        READ_REPOSITORY_ADMIN,
        Protocol["release evidence-show"],
        ["release_evidence_show"],
        delivery::Show,
        delivery::Page
    ),
    operation!(
        "decision.search",
        "Search permanent repository decisions.",
        READ_REPOSITORY_VIEWER,
        Protocol["decision search"],
        ["decision_search"],
        params::DecisionSearch,
        results::DecisionSearch
    ),
    operation!(
        "decision.record",
        "Record a permanent repository decision.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["decision record"],
        ["decision_record"],
        params::DecisionRecord,
        results::DecisionRecorded
    ),
    operation!(
        "decision.summarize",
        "Store a new rolling decision summary.",
        APPEND_REPOSITORY_ADMIN,
        Protocol["decision summarize"],
        ["decision_summarize"],
        params::DecisionSummarize,
        results::DecisionSummarized
    ),
    operation!(
        "usage.repositories",
        "Show privacy-preserving usage across visible repositories.",
        READ_REPOSITORY_OPERATOR,
        excluded "Usage analytics are presented by the Console, not the preserved CLI.",
        [],
        params::UsageRepositories,
        results::UsageRepositories
    ),
    operation!(
        "usage.repository",
        "Show privacy-preserving usage for one repository.",
        READ_REPOSITORY_OPERATOR,
        excluded "Usage analytics are presented by the Console, not the preserved CLI.",
        [],
        params::UsageRepository,
        results::UsageRepository
    ),
    operation!(
        "progress.repositories",
        "Show delivery progress across visible repositories.",
        READ_REPOSITORY_OPERATOR,
        excluded "Progress analytics are presented by the Console, not the preserved CLI.",
        [],
        params::Empty,
        results::ProgressRepositories
    ),
    operation!(
        "progress.repository",
        "Show delivery progress for one repository.",
        READ_REPOSITORY_OPERATOR,
        excluded "Progress analytics are presented by the Console, not the preserved CLI.",
        [],
        params::ProgressRepository,
        results::ProgressRepository
    ),
    operation!(
        "telegram.link",
        "Link one Telegram chat to an identity.",
        EXTERNAL_SELF,
        Protocol["telegram link"],
        [],
        params::TelegramLink,
        results::TelegramLinked
    ),
    operation!(
        "telegram.subscribe",
        "Subscribe one linked chat to notices.",
        EXTERNAL_SELF,
        Protocol["telegram subscribe"],
        [],
        params::TelegramSubscription,
        results::TelegramSubscription
    ),
    operation!(
        "telegram.unsubscribe",
        "Remove one notice subscription.",
        IDEMPOTENT_EXTERNAL_SELF,
        Protocol["telegram unsubscribe"],
        [],
        params::TelegramSubscription,
        results::TelegramSubscription
    ),
    operation!(
        "telegram.list",
        "List the caller's linked chats and subscriptions.",
        READ_SELF,
        Protocol["telegram list"],
        [],
        params::Empty,
        results::TelegramList
    ),
    operation!(
        "event.wait",
        "Wait for authorized owned-state events or per-filter heartbeat deadlines.",
        READ_SELF,
        Protocol["event wait"],
        ["event_wait"],
        params::EventWait,
        results::EventWaitResult
    ),
    operation!(
        "bug.report",
        "Report or count a Coordinator defect while the daemon may be unavailable.",
        APPEND_SELF,
        Local["bug report"],
        ["bug_report"],
        params::BugReport,
        results::BugRecord
    ),
    operation!(
        "bug.list",
        "List open Coordinator defects.",
        READ_SELF,
        Local["bug list"],
        ["bug_list"],
        params::Empty,
        results::BugList
    ),
    operation!(
        "bug.close",
        "Close one Coordinator defect.",
        DESTRUCTIVE_SELF,
        Local["bug close"],
        ["bug_close"],
        params::BugClose,
        results::BugClosed
    ),
];

pub fn operation(name: &str) -> Option<&'static OperationDefinition> {
    OPERATIONS.iter().find(|definition| definition.name == name)
}

pub fn operation_for_cli_route(
    route: &str,
) -> Option<(&'static OperationDefinition, &'static CliRouteDefinition)> {
    OPERATIONS.iter().find_map(|operation| {
        operation
            .cli_routes
            .iter()
            .find(|candidate| candidate.route == route)
            .map(|candidate| (operation, candidate))
    })
}

pub fn mcp_tools() -> Vec<McpToolDefinition> {
    let mut tools = Vec::new();
    for operation in OPERATIONS {
        tools.extend(
            operation
                .mcp_names
                .iter()
                .copied()
                .map(|name| McpToolDefinition {
                    name,
                    operation,
                    input_schema: operation.input_schema,
                    validate_params: operation.validate_params,
                    transform: McpArgumentTransform::Identity,
                }),
        );
        tools.extend(
            operation
                .mcp_overrides
                .iter()
                .map(|tool| McpToolDefinition {
                    name: tool.name,
                    operation,
                    input_schema: tool.input_schema,
                    validate_params: tool.validate_params,
                    transform: tool.transform,
                }),
        );
    }
    tools.sort_by_key(|tool| tool.name);
    tools
}

pub fn mcp_tool(name: &str) -> Option<McpToolDefinition> {
    mcp_tools().into_iter().find(|tool| tool.name == name)
}

pub fn parse_request(raw: &[u8]) -> Result<RequestEnvelope, ProtocolError> {
    if raw.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::RequestTooLarge,
            "request exceeds 64 KiB frame cap",
        ));
    }
    let mut value: Value = serde_json::from_slice(raw).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "request is not valid JSON")
            .with_detail(error.to_string())
    })?;
    let protocol = value.get("protocol").and_then(Value::as_u64);
    if protocol != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolUnsupported,
            format!(
                "protocol {} is not supported; install a protocol-2 client",
                protocol.map_or_else(|| "missing".to_owned(), |v| v.to_string())
            ),
        ));
    }
    work_context::sanitize_request(&mut value);
    let request: RequestEnvelope = serde_json::from_value(value).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "request envelope is invalid")
            .with_detail(error.to_string())
    })?;
    if request.id.is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "missing request id",
        ));
    }
    if request.operation.is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "missing operation",
        ));
    }
    if !request.params.is_object() {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "params must be an object",
        ));
    }
    let Some(definition) = operation(&request.operation) else {
        return Err(ProtocolError::new(
            ErrorCode::OperationUnknown,
            format!("unknown operation {:?}", request.operation),
        ));
    };
    if let Some(identity) = &request.client.identity
        && (identity.len() > 254 || !identity.contains('@'))
    {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "client.identity must be an e-mail",
        ));
    }
    (definition.validate_params)(&request.params)?;
    Ok(request)
}

pub fn encode_response(response: &ResponseEnvelope) -> Vec<u8> {
    let mut encoded = serde_json::to_vec(response).expect("response envelope is serializable");
    encoded.push(b'\n');
    if encoded.len() <= MAX_RESPONSE_BYTES {
        return encoded;
    }
    let fallback = ResponseEnvelope::failure(
        response_id(response),
        ProtocolError::new(ErrorCode::InternalError, "response exceeded size cap"),
    );
    let mut bounded = serde_json::to_vec(&fallback).expect("fallback is serializable");
    bounded.push(b'\n');
    bounded
}

fn response_id(response: &ResponseEnvelope) -> &str {
    match response {
        ResponseEnvelope::Success { id, .. } | ResponseEnvelope::Failure { id, .. } => id,
    }
}

pub fn contract_document() -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "product": "devcoordinator2",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL_VERSION,
        "transport": {
            "framing": "one newline-terminated UTF-8 JSON request and response per Unix connection",
            "maxRequestBytes": MAX_REQUEST_BYTES,
            "maxResponseBytes": MAX_RESPONSE_BYTES,
            "readTimeoutSeconds": READ_TIMEOUT_SECONDS,
            "writeTimeoutSeconds": WRITE_TIMEOUT_SECONDS
        },
        "requestEnvelope": schema_for::<RequestEnvelope>(),
        "operations": OPERATIONS.iter().map(|definition| serde_json::json!({
            "name": definition.name,
            "description": definition.description,
            "policy": definition.policy,
            "cli": {
                "routes": definition.cli_routes.iter().map(|route| serde_json::json!({
                    "route": route.route,
                    "dispatch": route.dispatch,
                })).collect::<Vec<_>>(),
                "excludedReason": definition.cli_exclusion,
            },
            "mcpExcludedReason": definition.mcp_exclusion,
            "mcpTools": definition.mcp_names.iter().map(|name| serde_json::json!({
                "name": name,
                "transform": "identity",
                "inputSchema": (definition.input_schema)()
            })).chain(definition.mcp_overrides.iter().map(|tool| serde_json::json!({
                "name": tool.name,
                "transform": match tool.transform {
                    McpArgumentTransform::Identity => "identity",
                    McpArgumentTransform::ClearCapacity => "clear_capacity"
                },
                "inputSchema": (tool.input_schema)()
            }))).collect::<Vec<_>>(),
            "inputSchema": (definition.input_schema)(),
            "outputSchema": (definition.output_schema)()
        })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_two_round_trip_is_strict() {
        let request = br#"{"protocol":2,"id":"abc","operation":"ping","params":{},"client":{"kind":"codex"}}"#;
        let parsed = parse_request(request).expect("valid request");
        assert_eq!(parsed.operation, "ping");
        assert_eq!(parsed.client.kind, ClientKind::Codex);

        let unknown =
            br#"{"protocol":2,"id":"abc","operation":"ping","params":{},"client":{},"extra":true}"#;
        assert_eq!(
            parse_request(unknown).unwrap_err().code,
            ErrorCode::ProtocolInvalid
        );
    }

    #[test]
    fn protocol_one_is_rejection_only() {
        let request = br#"{"protocol":1,"id":"old","command":"ping","args":{},"client":{}}"#;
        let error = parse_request(request).unwrap_err();
        assert_eq!(error.code, ErrorCode::ProtocolUnsupported);
    }

    #[test]
    fn operation_parameters_reject_unknown_fields_before_dispatch() {
        let request = br#"{"protocol":2,"id":"abc","operation":"user.whoami","params":{"extra":true},"client":{}}"#;
        assert_eq!(
            parse_request(request).unwrap_err().code,
            ErrorCode::ParamsInvalid
        );

        let evidence = br#"{"protocol":2,"id":"abc","operation":"test.evidence.get","params":{"path":"/repo","run_id":"run","extra":true},"client":{}}"#;
        assert_eq!(
            parse_request(evidence).unwrap_err().code,
            ErrorCode::ParamsInvalid
        );
        let mark = br##"{"protocol":2,"id":"abc","operation":"test.evidence.feedback.create","params":{"path":"/repo","run_id":"run","image_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","body":"change this","marks":[{"type":"pin","id":"mark-1","color":"#ef4444","x":0.5,"y":0.5,"extra":true}]},"client":{}}"##;
        assert_eq!(
            parse_request(mark).unwrap_err().code,
            ErrorCode::ParamsInvalid
        );
    }

    #[test]
    fn contract_has_one_typed_ping_definition() {
        let document = contract_document();
        assert_eq!(document["protocol"], 2);
        assert_eq!(document["operations"][0]["name"], "ping");
        assert!(document["operations"][0]["inputSchema"].is_object());
        assert!(document["operations"][0]["outputSchema"].is_object());
    }

    #[test]
    fn repository_health_schema_preserves_component_attribution() {
        let operation = operation("health.repository").expect("health repository operation");
        let schema = serde_json::to_string(&(operation.output_schema)()).unwrap();
        for field in [
            "deployment_id",
            "component",
            "component_type",
            "name",
            "image",
            "state",
            "binding",
            "container_layer",
            "pg_connections",
            "pg_wal_bytes",
            "pg_temp_bytes",
            "pg_database_bytes",
        ] {
            assert!(
                schema.contains(&format!("\"{field}\"")),
                "health repository output omitted {field}"
            );
        }
    }

    #[test]
    fn registry_names_and_mcp_tools_are_unique() {
        let mut operations = std::collections::BTreeSet::new();
        let mut tools = std::collections::BTreeSet::new();
        let mut cli_routes = std::collections::BTreeSet::new();
        for definition in OPERATIONS {
            assert!(operations.insert(definition.name), "duplicate operation");
            assert_eq!(
                definition.cli_routes.is_empty(),
                definition.cli_exclusion.is_some(),
                "{} must declare routes or one deliberate CLI exclusion",
                definition.name
            );
            for route in definition.cli_routes {
                assert!(!route.route.is_empty());
                assert!(cli_routes.insert(route.route), "duplicate CLI route");
                assert_eq!(
                    operation_for_cli_route(route.route)
                        .map(|(operation, candidate)| (operation.name, candidate.dispatch)),
                    Some((definition.name, route.dispatch))
                );
            }
            assert_eq!(
                definition.mcp_names.is_empty() && definition.mcp_overrides.is_empty(),
                definition.mcp_exclusion.is_some(),
                "{} must declare MCP tools or one deliberate exclusion",
                definition.name
            );
        }
        for tool in mcp_tools() {
            assert!(tools.insert(tool.name), "duplicate MCP tool");
        }
        assert_eq!(OPERATIONS.len(), 100);
        assert_eq!(tools.len(), 77);
        assert_eq!(cli_routes.len(), 89);
    }

    #[test]
    fn clear_capacity_tool_has_empty_input_and_normalizes_to_null() {
        let clear = mcp_tool("test_capacity_clear").expect("clear tool");
        (clear.validate_params)(&serde_json::json!({})).expect("empty params");
        assert_eq!(
            clear.transform.apply(serde_json::json!({})),
            serde_json::json!({"cap": null})
        );
        assert_eq!(clear.operation.name, "test.capacity.set");
    }

    #[test]
    fn every_operation_has_a_strict_concrete_contract() {
        let contract = contract_document();
        for operation in contract["operations"].as_array().expect("operations") {
            assert_eq!(
                operation["inputSchema"]["additionalProperties"], false,
                "{} input is not strict",
                operation["name"]
            );
            let output = operation["outputSchema"].to_string();
            assert!(!output.contains("ObjectData"));
            assert!(!output.contains("PendingDomainResult"));
        }
    }
}
