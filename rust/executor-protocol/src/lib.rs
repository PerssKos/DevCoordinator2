//! Strict, versioned contracts for the DevCoordinator2 Rust execution plane.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const EXECUTOR_SCHEMA: u8 = 2;
pub const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 4096;
pub const MAX_CASES: usize = 4096;
pub const MAX_FAILURE_INDEX: usize = 128;
pub const MAX_REASON_BYTES: usize = 512;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_CASE_ARG_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_ARGS: usize = 256;
pub const MAX_CASE_ARGS: usize = 64;
pub const MAX_DIAGNOSTIC_SOURCES: usize = 8;
pub const MAX_DIAGNOSTIC_EVENTS: usize = 4096;
pub const MAX_DIAGNOSTIC_PREVIEW_BYTES: usize = 256;
pub const MAX_DIAGNOSTIC_NAME_BYTES: usize = 256;
pub const MAX_DIAGNOSTIC_PATH_BYTES: usize = 512;
pub const MAX_DIAGNOSTIC_LOG_REFS: usize = 16;
pub const MAX_RETAINED_ARTIFACTS: usize = 8;
pub const MAX_RETAINED_ARTIFACT_FILES: usize = 4096;
pub const MAX_RETAINED_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_RETAINED_ARTIFACT_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RETAINED_ARTIFACT_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

/// A schema marker that can only deserialize the current executor contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Schema2;

impl Serialize for Schema2 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(EXECUTOR_SCHEMA)
    }
}

impl<'de> Deserialize<'de> for Schema2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        if value == EXECUTOR_SCHEMA {
            Ok(Self)
        } else {
            Err(de::Error::custom(format!(
                "executor schema must be {EXECUTOR_SCHEMA}; schema {value} is unsupported"
            )))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationTier {
    Development,
    PreMerge,
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    Complete,
    Selected,
    Retry,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRole {
    Work,
    Preflight,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckPhase {
    Setup,
    Build,
    Fixture,
    Case,
    Cleanup,
    Qualification,
    #[default]
    Check,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Database,
    Port,
    Directory,
    SourceSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceClaim {
    pub kind: ResourceKind,
    pub id: String,
    pub access: ResourceAccess,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumedArtifact {
    pub check: String,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionMode {
    Process,
    Event,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticReportFormat {
    Junit,
    PlaywrightJson,
    RustJson,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticReportSource {
    pub format: DiagnosticReportFormat,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogPhase {
    Executor,
    Check,
    Discovery,
    Case,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogRef {
    pub run_id: String,
    pub check: Option<String>,
    pub phase: LogPhase,
    pub case: Option<String>,
    pub stream: LogStream,
}

impl LogRef {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_identity("log run_id", &self.run_id, 128)?;
        match self.phase {
            LogPhase::Executor => {
                if self.check.is_some() || self.case.is_some() {
                    return Err(ContractError::new(
                        "executor log references cannot name a check or case",
                    ));
                }
            }
            LogPhase::Check | LogPhase::Discovery => {
                let Some(check) = &self.check else {
                    return Err(ContractError::new(
                        "check and discovery log references require a check",
                    ));
                };
                validate_name("log check", check, 64)?;
                if self.case.is_some() {
                    return Err(ContractError::new(
                        "only case log references may name a case",
                    ));
                }
            }
            LogPhase::Case => {
                let Some(check) = &self.check else {
                    return Err(ContractError::new("case log references require a check"));
                };
                let Some(case) = &self.case else {
                    return Err(ContractError::new("case log references require a case"));
                };
                validate_name("log check", check, 64)?;
                validate_case_id(case)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Assertion,
    Compiler,
    Panic,
    Exception,
    StackFrame,
    Timeout,
    Cancellation,
    ProcessExit,
    BrowserConsole,
    NetworkRequest,
    StructuredEvidenceInvalid,
    LogStorage,
    SourceChanged,
    Artifact,
    Dependency,
    Internal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    DeadlineExceeded,
    UserCancelled,
    Superseded,
    RunCancelled,
    DaemonInterrupted,
    UnsafeStop,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOrigin {
    ExplicitEvent,
    Junit,
    PlaywrightJson,
    RustJson,
    Executor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticValueType {
    Null,
    Boolean,
    Number,
    String,
    Json,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticValue {
    #[serde(rename = "type")]
    pub value_type: DiagnosticValueType,
    pub preview: Option<String>,
    pub sha256: String,
    pub byte_count: u64,
    pub truncated: bool,
    pub redacted: bool,
}

impl DiagnosticValue {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_digest("diagnostic value sha256", &self.sha256)?;
        if self.redacted && self.preview.is_some() {
            return Err(ContractError::new(
                "redacted diagnostic values cannot include a preview",
            ));
        }
        if let Some(preview) = &self.preview
            && (preview.len() > MAX_DIAGNOSTIC_PREVIEW_BYTES
                || preview.chars().any(char::is_control)
                || u64::try_from(preview.len()).unwrap_or(u64::MAX) > self.byte_count)
        {
            return Err(ContractError::new(
                "diagnostic value preview is not a bounded single-line value",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
}

impl SourceLocation {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.file.len() > MAX_DIAGNOSTIC_PATH_BYTES {
            return Err(ContractError::new(
                "diagnostic source path exceeds 512 bytes",
            ));
        }
        validate_relative_path("diagnostic source", &self.file)?;
        if self.line == 0 || self.column == Some(0) {
            return Err(ContractError::new(
                "diagnostic source line and column are one-based",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticExit {
    pub code: Option<i32>,
    pub signal: Option<u8>,
}

impl DiagnosticExit {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.code.is_some() && self.signal.is_some() {
            return Err(ContractError::new(
                "diagnostic exit cannot contain both a code and signal",
            ));
        }
        if self.signal == Some(0) || self.signal.is_some_and(|signal| signal > 127) {
            return Err(ContractError::new("diagnostic signal must be in [1, 127]"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactSpec {
    pub name: String,
    pub path: String,
    pub max_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactReceipt {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseSpec {
    pub id: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub postgres: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckPlan {
    pub name: String,
    pub tier: ValidationTier,
    pub role: CheckRole,
    #[serde(default)]
    pub phase: CheckPhase,
    #[serde(default)]
    pub resources: Vec<ResourceClaim>,
    pub after: Vec<String>,
    pub requires: Vec<String>,
    pub invalidates: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: Option<u64>,
    pub completion: CompletionMode,
    pub on_failure: FailureMode,
    pub produces: Vec<String>,
    #[serde(default)]
    pub consumes: Vec<ConsumedArtifact>,
    #[serde(default)]
    pub cacheable: bool,
    #[serde(default)]
    pub cache_inputs: Vec<String>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub expect_failure: bool,
    #[serde(default)]
    pub qualification_of: Option<String>,
    #[serde(default)]
    pub retained_artifacts: Vec<RetainedArtifactSpec>,
    #[serde(default)]
    pub diagnostic_sources: Vec<DiagnosticReportSource>,
    pub command: Option<Vec<String>>,
    pub discover: Option<Vec<String>>,
    pub case_command: Option<Vec<String>>,
    pub cases: Option<Vec<CaseSpec>>,
}

impl CheckPlan {
    pub fn is_fanout(&self) -> bool {
        self.case_command.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlan {
    pub schema: Schema2,
    pub run_id: String,
    pub test: String,
    pub worktree_root: String,
    pub current_dir: String,
    pub log_dir: String,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub selection: Vec<String>,
    pub origin_run_id: Option<String>,
    pub source_digest: String,
    pub config_digest: String,
    pub reused: BTreeMap<String, Vec<ArtifactReceipt>>,
    #[serde(default)]
    pub reused_qualifications: BTreeSet<String>,
    #[serde(default)]
    pub case_selection: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub postgres_databases: BTreeMap<String, String>,
    pub checks: Vec<CheckPlan>,
}

impl ExecutionPlan {
    pub fn from_json(input: &[u8]) -> Result<Self, ContractError> {
        if input.len() > MAX_REPORT_BYTES {
            return Err(ContractError::new("executor plan exceeds 2 MiB"));
        }
        let plan: Self = serde_json::from_slice(input)
            .map_err(|error| ContractError::new(format!("invalid executor JSON: {error}")))?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn from_toml(input: &str) -> Result<Self, ContractError> {
        if input.len() > MAX_REPORT_BYTES {
            return Err(ContractError::new("executor plan exceeds 2 MiB"));
        }
        let plan: Self = toml::from_str(input)
            .map_err(|error| ContractError::new(format!("invalid executor TOML: {error}")))?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn active_checks(&self) -> impl Iterator<Item = &CheckPlan> {
        self.checks
            .iter()
            .filter(|check| check.tier <= self.requested_tier)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        validate_identity("run_id", &self.run_id, 128)?;
        validate_name("test", &self.test, 32)?;
        validate_digest("source_digest", &self.source_digest)?;
        validate_digest("config_digest", &self.config_digest)?;
        if self.checks.is_empty() {
            return Err(ContractError::new("executor plan has no checks"));
        }
        if self.checks.len() > 256 {
            return Err(ContractError::new("executor plan exceeds 256 checks"));
        }
        if self.origin_run_id.is_some() != (self.proof == ProofKind::Retry) {
            return Err(ContractError::new(
                "origin_run_id is required only for retry proof",
            ));
        }
        if let Some(origin) = &self.origin_run_id {
            validate_identity("origin_run_id", origin, 128)?;
        }
        let expected_readiness = self.requested_tier == ValidationTier::Release
            && self.proof == ProofKind::Complete
            && self.selection.is_empty();
        if self.readiness_eligible != expected_readiness {
            return Err(ContractError::new(
                "readiness_eligible does not match tier, proof, and selection",
            ));
        }
        if self.proof == ProofKind::Complete && !self.selection.is_empty() {
            return Err(ContractError::new(
                "complete proof cannot carry a check selection",
            ));
        }
        if self.proof != ProofKind::Complete && self.selection.is_empty() {
            return Err(ContractError::new(
                "selected and retry proofs require a non-empty selection",
            ));
        }
        if !Path::new(&self.worktree_root).is_absolute()
            || !Path::new(&self.current_dir).is_absolute()
            || !Path::new(&self.log_dir).is_absolute()
        {
            return Err(ContractError::new(
                "worktree_root, current_dir, and log_dir must be absolute paths",
            ));
        }
        let normalized_root = normalize_absolute_path(Path::new(&self.worktree_root))
            .ok_or_else(|| ContractError::new("worktree_root cannot escape its filesystem root"))?;
        let normalized_log = normalize_absolute_path(Path::new(&self.log_dir))
            .ok_or_else(|| ContractError::new("log_dir cannot escape its filesystem root"))?;
        if normalized_log == normalized_root || !normalized_log.starts_with(&normalized_root) {
            return Err(ContractError::new(
                "log_dir must be contained below worktree_root",
            ));
        }

        let mut names = BTreeMap::new();
        for (index, check) in self.checks.iter().enumerate() {
            validate_check(check)?;
            if names.insert(check.name.as_str(), index).is_some() {
                return Err(ContractError::new(format!(
                    "duplicate check name {:?}",
                    check.name
                )));
            }
        }

        let selected: BTreeSet<&str> = self.selection.iter().map(String::as_str).collect();
        if selected.len() != self.selection.len() {
            return Err(ContractError::new("selection contains duplicate checks"));
        }
        if self.selection.len() > 256 {
            return Err(ContractError::new("selection exceeds 256 checks"));
        }
        for name in &self.selection {
            if !names.contains_key(name.as_str()) {
                return Err(ContractError::new(format!(
                    "selection references unknown check {name:?}"
                )));
            }
        }
        for (name, cases) in &self.case_selection {
            let Some(index) = names.get(name.as_str()) else {
                return Err(ContractError::new(format!(
                    "case selection references unknown check {name:?}"
                )));
            };
            if !selected.contains(name.as_str()) || !self.checks[*index].is_fanout() {
                return Err(ContractError::new(format!(
                    "case selection {name:?} must name a selected fan-out check"
                )));
            }
            validate_case_selection(cases)?;
            if let Some(declared) = &self.checks[*index].cases {
                let declared = declared
                    .iter()
                    .map(|case| case.id.as_str())
                    .collect::<BTreeSet<_>>();
                if cases.iter().any(|case| !declared.contains(case.as_str())) {
                    return Err(ContractError::new(format!(
                        "case selection {name:?} names an undeclared static case"
                    )));
                }
            }
        }
        for (branch, database) in &self.postgres_databases {
            validate_name("PostgreSQL branch", branch, 32)?;
            validate_name("PostgreSQL database", database, 63)?;
        }
        for check in &self.checks {
            if let Some(cases) = &check.cases {
                for case in cases {
                    if case
                        .postgres
                        .as_ref()
                        .is_some_and(|branch| !self.postgres_databases.contains_key(branch))
                    {
                        return Err(ContractError::new(format!(
                            "case {:?} references an unknown PostgreSQL branch",
                            case.id
                        )));
                    }
                }
            }
        }
        for (name, receipts) in &self.reused {
            let Some(index) = names.get(name.as_str()) else {
                return Err(ContractError::new(format!(
                    "reused evidence references unknown check {name:?}"
                )));
            };
            if self.checks[*index].tier > self.requested_tier {
                return Err(ContractError::new(format!(
                    "reused evidence references inactive check {name:?}"
                )));
            }
            if receipts.is_empty() {
                return Err(ContractError::new(format!(
                    "reused check {name:?} has no artifact receipts"
                )));
            }
            for receipt in receipts {
                validate_relative_path("artifact", &receipt.path)?;
                validate_digest("artifact sha256", &receipt.sha256)?;
            }
        }
        for name in &self.reused_qualifications {
            let Some(index) = names.get(name.as_str()) else {
                return Err(ContractError::new(format!(
                    "qualification reuse references unknown check {name:?}"
                )));
            };
            let check = &self.checks[*index];
            if check.phase != CheckPhase::Qualification || !check.cacheable {
                return Err(ContractError::new(format!(
                    "qualification reuse {name:?} is not a cacheable qualification"
                )));
            }
        }

        let mut graph = vec![Vec::<usize>::new(); self.checks.len()];
        let mut compiled_edges = BTreeSet::new();
        let mut requires_edges = BTreeSet::new();
        for (index, check) in self.checks.iter().enumerate() {
            let mut local_edges = BTreeSet::new();
            for dependency in check.after.iter().chain(&check.requires) {
                let Some(dep_index) = names.get(dependency.as_str()) else {
                    return Err(ContractError::new(format!(
                        "check {:?} references unknown dependency {dependency:?}",
                        check.name
                    )));
                };
                if *dep_index == index {
                    return Err(ContractError::new(format!(
                        "check {:?} depends on itself",
                        check.name
                    )));
                }
                if !local_edges.insert(*dep_index) {
                    return Err(ContractError::new(format!(
                        "check {:?} repeats dependency {dependency:?}",
                        check.name
                    )));
                }
                if self.checks[*dep_index].tier > check.tier {
                    return Err(ContractError::new(format!(
                        "check {:?} depends on higher-tier check {dependency:?}",
                        check.name
                    )));
                }
                graph[*dep_index].push(index);
                compiled_edges.insert((*dep_index, index));
            }
            for dependency in &check.requires {
                let dep_index = names[dependency.as_str()];
                requires_edges.insert((dep_index, index));
            }
            for consumed in &check.consumes {
                let Some(producer_index) = names.get(consumed.check.as_str()) else {
                    return Err(ContractError::new(format!(
                        "check {:?} consumes from unknown check {:?}",
                        check.name, consumed.check
                    )));
                };
                if !check.requires.contains(&consumed.check)
                    || !self.checks[*producer_index]
                        .produces
                        .contains(&consumed.path)
                {
                    return Err(ContractError::new(format!(
                        "check {:?} consumed artifact {:?} is not a required producer output",
                        check.name, consumed.path
                    )));
                }
            }
        }
        for (preflight_index, preflight) in self.checks.iter().enumerate() {
            if let Some(target) = &preflight.qualification_of
                && !preflight.invalidates.contains(target)
            {
                return Err(ContractError::new(format!(
                    "qualification {:?} must invalidate its target {target:?}",
                    preflight.name
                )));
            }
            for target in &preflight.invalidates {
                let Some(target_index) = names.get(target.as_str()) else {
                    return Err(ContractError::new(format!(
                        "preflight {:?} invalidates unknown check {target:?}",
                        preflight.name
                    )));
                };
                if *target_index == preflight_index {
                    return Err(ContractError::new(format!(
                        "preflight {:?} invalidates itself",
                        preflight.name
                    )));
                }
                if self.checks[*target_index].tier < preflight.tier {
                    return Err(ContractError::new(format!(
                        "preflight {:?} invalidates lower-tier check {target:?}",
                        preflight.name
                    )));
                }
                if !compiled_edges.insert((preflight_index, *target_index))
                    && !requires_edges.contains(&(preflight_index, *target_index))
                {
                    return Err(ContractError::new(format!(
                        "preflight {:?} duplicates a dependency edge to {target:?}",
                        preflight.name
                    )));
                }
                if !requires_edges.contains(&(preflight_index, *target_index)) {
                    graph[preflight_index].push(*target_index);
                }
            }
        }
        ensure_acyclic(&graph, &self.checks)?;
        Ok(())
    }
}

fn validate_check(check: &CheckPlan) -> Result<(), ContractError> {
    validate_name("check", &check.name, 64)?;
    if check.resources.len() > 32 {
        return Err(ContractError::new(format!(
            "check {:?} declares more than 32 resource claims",
            check.name
        )));
    }
    let mut resources = BTreeSet::new();
    for resource in &check.resources {
        validate_identity("resource id", &resource.id, 128)?;
        if !resources.insert((resource.kind, resource.id.as_str())) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a resource claim",
                check.name
            )));
        }
    }
    if check.cwd != "." {
        validate_relative_path("cwd", &check.cwd)?;
    }
    if let Some(timeout_seconds) = check.timeout_seconds
        && !(1..=21_600).contains(&timeout_seconds)
    {
        return Err(ContractError::new(format!(
            "check {:?} timeout_seconds must be null or in [1, 21600]",
            check.name
        )));
    }
    let direct = check.command.is_some();
    let dynamic = check.discover.is_some();
    let static_cases = check.cases.is_some();
    let fanout = check.case_command.is_some();
    if direct == fanout
        || (!fanout && (dynamic || static_cases))
        || (fanout && dynamic == static_cases)
    {
        return Err(ContractError::new(format!(
            "check {:?} must declare exactly one command or one discover/static fan-out",
            check.name
        )));
    }
    for command in [&check.command, &check.discover, &check.case_command]
        .into_iter()
        .flatten()
    {
        validate_command(&check.name, command)?;
    }
    if fanout && check.completion != CompletionMode::Process {
        return Err(ContractError::new(format!(
            "fan-out check {:?} must use process completion",
            check.name
        )));
    }
    if check.role == CheckRole::Preflight && check.completion != CompletionMode::Process {
        return Err(ContractError::new(format!(
            "preflight {:?} must use process completion",
            check.name
        )));
    }
    if check.role != CheckRole::Preflight && !check.invalidates.is_empty() {
        return Err(ContractError::new(format!(
            "work check {:?} cannot invalidate other checks",
            check.name
        )));
    }
    let invalidates: BTreeSet<&str> = check.invalidates.iter().map(String::as_str).collect();
    if invalidates.len() != check.invalidates.len() {
        return Err(ContractError::new(format!(
            "preflight {:?} repeats an invalidated check",
            check.name
        )));
    }
    for key in check.env.keys() {
        if key.starts_with("DEVCOORDINATOR_") {
            return Err(ContractError::new(format!(
                "check {:?} overrides reserved environment variable {key:?}",
                check.name
            )));
        }
        if key.is_empty() || key.as_bytes().contains(&b'=') || key.as_bytes().contains(&0) {
            return Err(ContractError::new(format!(
                "check {:?} contains an invalid environment name",
                check.name
            )));
        }
    }
    let produces: BTreeSet<&str> = check.produces.iter().map(String::as_str).collect();
    if check.produces.len() > 16 {
        return Err(ContractError::new(format!(
            "check {:?} declares more than 16 artifacts",
            check.name
        )));
    }
    if produces.len() != check.produces.len() {
        return Err(ContractError::new(format!(
            "check {:?} repeats an artifact path",
            check.name
        )));
    }
    for path in &check.produces {
        validate_relative_path("artifact", path)?;
    }
    if check.consumes.len() > 16 {
        return Err(ContractError::new(format!(
            "check {:?} consumes more than 16 artifacts",
            check.name
        )));
    }
    let mut consumes = BTreeSet::new();
    for consumed in &check.consumes {
        validate_name("consumed artifact check", &consumed.check, 64)?;
        validate_relative_path("consumed artifact", &consumed.path)?;
        if !consumes.insert((consumed.check.as_str(), consumed.path.as_str())) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a consumed artifact",
                check.name
            )));
        }
    }
    if check.cache_inputs.len() > 32 {
        return Err(ContractError::new(format!(
            "check {:?} declares more than 32 cache inputs",
            check.name
        )));
    }
    let mut cache_inputs = BTreeSet::new();
    for path in &check.cache_inputs {
        validate_relative_path("cache input", path)?;
        if !cache_inputs.insert(path.as_str()) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a cache input",
                check.name
            )));
        }
    }
    if !check.fingerprint.is_empty() {
        validate_digest("check fingerprint", &check.fingerprint)?;
    }
    if check.cacheable
        && (check.completion != CompletionMode::Process
            || check.command.is_none()
            || check.cache_inputs.is_empty()
            || (check.produces.is_empty() && check.phase != CheckPhase::Qualification)
            || check.fingerprint.is_empty())
    {
        return Err(ContractError::new(format!(
            "cacheable check {:?} requires a direct process, cache inputs, fingerprint, and outputs unless it is a qualification",
            check.name
        )));
    }
    if check.expect_failure != (check.phase == CheckPhase::Qualification)
        || (check.phase == CheckPhase::Qualification) != check.qualification_of.is_some()
    {
        return Err(ContractError::new(format!(
            "qualification check {:?} must name its target and expect failure",
            check.name
        )));
    }
    if let Some(target) = &check.qualification_of {
        validate_name("qualification target", target, 64)?;
    }
    if check.retained_artifacts.len() > MAX_RETAINED_ARTIFACTS {
        return Err(ContractError::new(format!(
            "check {:?} declares more than {MAX_RETAINED_ARTIFACTS} retained artifacts",
            check.name
        )));
    }
    if fanout && !check.retained_artifacts.is_empty() {
        return Err(ContractError::new(format!(
            "fan-out check {:?} cannot declare retained artifacts",
            check.name
        )));
    }
    if check.completion != CompletionMode::Process && !check.retained_artifacts.is_empty() {
        return Err(ContractError::new(format!(
            "event-completed check {:?} cannot declare retained artifacts",
            check.name
        )));
    }
    let mut retained_names = BTreeSet::new();
    let mut retained_paths = BTreeSet::new();
    let mut retained_total = 0_u64;
    for artifact in &check.retained_artifacts {
        validate_name("retained artifact", &artifact.name, 64)?;
        validate_relative_path("retained artifact", &artifact.path)?;
        if artifact.path.len() > 256 || artifact.path.chars().any(char::is_control) {
            return Err(ContractError::new(format!(
                "check {:?} retained artifact path exceeds its safe bound",
                check.name
            )));
        }
        if artifact.path == ".git"
            || artifact.path.starts_with(".git/")
            || artifact.path == ".devcoordinator"
            || artifact.path.starts_with(".devcoordinator/")
        {
            return Err(ContractError::new(format!(
                "check {:?} retained artifact path is reserved",
                check.name
            )));
        }
        if artifact.max_bytes == 0 || artifact.max_bytes > MAX_RETAINED_ARTIFACT_BYTES {
            return Err(ContractError::new(format!(
                "check {:?} retained artifact {:?} max_bytes must be in [1, {MAX_RETAINED_ARTIFACT_BYTES}]",
                check.name, artifact.name
            )));
        }
        retained_total = retained_total
            .checked_add(artifact.max_bytes)
            .ok_or_else(|| ContractError::new("retained artifact byte limit overflow"))?;
        if !retained_names.insert(artifact.name.as_str()) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a retained artifact name",
                check.name
            )));
        }
        if !retained_paths.insert(artifact.path.as_str()) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a retained artifact path",
                check.name
            )));
        }
    }
    if retained_total > MAX_RETAINED_ARTIFACT_TOTAL_BYTES {
        return Err(ContractError::new(format!(
            "check {:?} retained artifact limits exceed {MAX_RETAINED_ARTIFACT_TOTAL_BYTES} bytes",
            check.name
        )));
    }
    for left in &check.retained_artifacts {
        for right in &check.retained_artifacts {
            if left.name != right.name && Path::new(&right.path).starts_with(Path::new(&left.path))
            {
                return Err(ContractError::new(format!(
                    "check {:?} retained artifact paths overlap",
                    check.name
                )));
            }
        }
    }
    if check.diagnostic_sources.len() > MAX_DIAGNOSTIC_SOURCES {
        return Err(ContractError::new(format!(
            "check {:?} declares more than {MAX_DIAGNOSTIC_SOURCES} diagnostic sources",
            check.name
        )));
    }
    let mut diagnostic_sources = BTreeSet::new();
    for source in &check.diagnostic_sources {
        if source.path.len() > MAX_DIAGNOSTIC_PATH_BYTES {
            return Err(ContractError::new(format!(
                "check {:?} diagnostic source path exceeds 512 bytes",
                check.name
            )));
        }
        validate_relative_path("diagnostic report", &source.path)?;
        if !diagnostic_sources.insert((source.format, source.path.as_str())) {
            return Err(ContractError::new(format!(
                "check {:?} repeats a diagnostic source",
                check.name
            )));
        }
    }
    if let Some(cases) = &check.cases {
        validate_cases(cases)?;
    }
    Ok(())
}

pub fn validate_cases(cases: &[CaseSpec]) -> Result<(), ContractError> {
    if cases.len() > MAX_CASES {
        return Err(ContractError::new(format!(
            "case manifest exceeds {MAX_CASES} cases"
        )));
    }
    let mut ids = BTreeSet::new();
    for case in cases {
        validate_case_id(&case.id)?;
        if !ids.insert(case.id.as_str()) {
            return Err(ContractError::new(format!(
                "duplicate case id {:?}",
                case.id
            )));
        }
        if case.args.len() > MAX_CASE_ARGS {
            return Err(ContractError::new(format!(
                "case {:?} has too many arguments",
                case.id
            )));
        }
        let mut total = 0usize;
        for arg in &case.args {
            if arg.is_empty() || arg.as_bytes().contains(&0) || arg.len() > MAX_ARG_BYTES {
                return Err(ContractError::new(format!(
                    "case {:?} contains an invalid argument",
                    case.id
                )));
            }
            total = total.saturating_add(arg.len());
        }
        if total > MAX_CASE_ARG_BYTES {
            return Err(ContractError::new(format!(
                "case {:?} arguments exceed 64 KiB",
                case.id
            )));
        }
        if let Some(postgres) = &case.postgres {
            validate_name("PostgreSQL case branch", postgres, 32)?;
        }
    }
    Ok(())
}

fn validate_case_selection(cases: &[String]) -> Result<(), ContractError> {
    if cases.is_empty() || cases.len() > MAX_CASES {
        return Err(ContractError::new(
            "case selection must contain 1..=4096 case ids",
        ));
    }
    let mut unique = BTreeSet::new();
    for case in cases {
        validate_case_id(case)?;
        if !unique.insert(case.as_str()) {
            return Err(ContractError::new("case selection contains duplicates"));
        }
    }
    Ok(())
}

fn validate_command(check: &str, command: &[String]) -> Result<(), ContractError> {
    if command.is_empty() || command.len() > MAX_COMMAND_ARGS {
        return Err(ContractError::new(format!(
            "check {check:?} command must contain 1..={MAX_COMMAND_ARGS} arguments"
        )));
    }
    for (index, arg) in command.iter().enumerate() {
        if index == 0 && arg.is_empty() {
            return Err(ContractError::new(format!(
                "check {check:?} command executable is empty"
            )));
        }
        if arg.as_bytes().contains(&0) || arg.len() > MAX_ARG_BYTES {
            return Err(ContractError::new(format!(
                "check {check:?} command contains an invalid argument"
            )));
        }
    }
    Ok(())
}

fn validate_name(kind: &str, value: &str, maximum: usize) -> Result<(), ContractError> {
    let valid = !value.is_empty()
        && value.len() <= maximum
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
        });
    if !valid {
        return Err(ContractError::new(format!(
            "{kind} {value:?} is not a normalized lower-case name"
        )));
    }
    Ok(())
}

fn validate_identity(kind: &str, value: &str, maximum: usize) -> Result<(), ContractError> {
    let valid = !value.is_empty()
        && value.len() <= maximum
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(ContractError::new(format!(
            "{kind} is not a bounded normalized identity"
        )));
    }
    Ok(())
}

fn validate_case_id(value: &str) -> Result<(), ContractError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'-' | b'_' | b'.'))
        });
    if !valid {
        return Err(ContractError::new(format!(
            "case id {value:?} is not path-safe"
        )));
    }
    Ok(())
}

fn validate_digest(kind: &str, value: &str) -> Result<(), ContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ContractError::new(format!(
            "{kind} must be 64 lower-case hexadecimal characters"
        )));
    }
    Ok(())
}

fn validate_fingerprint(value: &str) -> Result<(), ContractError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(ContractError::new(
            "diagnostic fingerprint must use the sha256 scheme",
        ));
    };
    validate_digest("diagnostic fingerprint", digest)
}

fn validate_diagnostic_name(kind: &str, value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > MAX_DIAGNOSTIC_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ContractError::new(format!(
            "{kind} is not a bounded single-line name"
        )));
    }
    Ok(())
}

fn validate_relative_path(kind: &str, value: &str) -> Result<(), ContractError> {
    let path = Path::new(value);
    let valid = !value.is_empty()
        && !path.is_absolute()
        && !value.contains('\\')
        && !value.as_bytes().contains(&0)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        return Err(ContractError::new(format!(
            "{kind} path {value:?} is not normalized repository-relative"
        )));
    }
    Ok(())
}

fn normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

fn ensure_acyclic(graph: &[Vec<usize>], checks: &[CheckPlan]) -> Result<(), ContractError> {
    fn visit(
        node: usize,
        graph: &[Vec<usize>],
        marks: &mut [u8],
        checks: &[CheckPlan],
    ) -> Result<(), ContractError> {
        if marks[node] == 1 {
            return Err(ContractError::new(format!(
                "check graph contains a cycle at {:?}",
                checks[node].name
            )));
        }
        if marks[node] == 2 {
            return Ok(());
        }
        marks[node] = 1;
        for &next in &graph[node] {
            visit(next, graph, marks, checks)?;
        }
        marks[node] = 2;
        Ok(())
    }

    let mut marks = vec![0; graph.len()];
    for node in 0..graph.len() {
        visit(node, graph, &mut marks, checks)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseManifest {
    pub schema: Schema2,
    pub cases: Vec<CaseSpec>,
}

impl CaseManifest {
    pub fn from_json(input: &[u8]) -> Result<Self, ContractError> {
        if input.len() > MAX_MANIFEST_BYTES {
            return Err(ContractError::new("case manifest exceeds 2 MiB"));
        }
        let manifest: Self = serde_json::from_slice(input)
            .map_err(|error| ContractError::new(format!("invalid case manifest: {error}")))?;
        validate_cases(&manifest.cases)?;
        Ok(manifest)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafStatus {
    Pending,
    Running,
    Reused,
    Passed,
    Failed,
    TimedOut,
    Invalidated,
    NotMeaningful,
    Cancelled,
    Unsafe,
}

impl LeafStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Passed | Self::Reused)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Passed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogStreamSummary {
    pub log_ref: LogRef,
    pub bytes: u64,
    pub lines: u64,
    pub sha256: String,
    pub first_write_epoch_ms: Option<u64>,
    pub last_write_epoch_ms: Option<u64>,
    pub complete: bool,
}

impl LogStreamSummary {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.log_ref.validate()?;
        validate_digest("log stream sha256", &self.sha256)?;
        if self.first_write_epoch_ms.is_some() != self.last_write_epoch_ms.is_some() {
            return Err(ContractError::new(
                "log stream first and last write times must appear together",
            ));
        }
        if self.bytes == 0 && self.first_write_epoch_ms.is_some() {
            return Err(ContractError::new(
                "empty log streams cannot have write times",
            ));
        }
        if self.bytes > 0 && self.first_write_epoch_ms.is_none() {
            return Err(ContractError::new(
                "non-empty log streams require write times",
            ));
        }
        if (self.bytes == 0 && self.lines != 0)
            || (self.bytes > 0 && (self.lines == 0 || self.lines > self.bytes))
        {
            return Err(ContractError::new(
                "log stream line count is inconsistent with its byte count",
            ));
        }
        if self
            .first_write_epoch_ms
            .zip(self.last_write_epoch_ms)
            .is_some_and(|(first, last)| last < first)
        {
            return Err(ContractError::new(
                "log stream last write precedes its first write",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProgress {
    pub waiting: u32,
    pub admitted: u32,
    pub executing: u32,
    pub finished: u32,
    pub queued_at_epoch_ms: u64,
    pub admitted_at_epoch_ms: Option<u64>,
    pub started_at_epoch_ms: Option<u64>,
    pub finished_at_epoch_ms: Option<u64>,
    pub capacity_wait_ms: u64,
    pub process_duration_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionProgress>,
    pub id: String,
    pub status: LeafStatus,
    pub exit: DiagnosticExit,
    pub duration_ms: u64,
    pub streams: Vec<LogStreamSummary>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionProgress>,
    pub name: String,
    pub tier: ValidationTier,
    pub role: CheckRole,
    #[serde(default)]
    pub phase: CheckPhase,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub cache_inputs: Vec<ArtifactReceipt>,
    #[serde(default)]
    pub consumed_artifacts: Vec<ArtifactReceipt>,
    pub status: LeafStatus,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub exit: DiagnosticExit,
    pub artifacts: Vec<ArtifactReceipt>,
    pub retained_artifacts: Vec<RetainedArtifactReceipt>,
    pub streams: Vec<LogStreamSummary>,
    pub case_count: u32,
    pub cases: Vec<CaseReport>,
    pub cases_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseDuration {
    pub phase: CheckPhase,
    pub duration_seconds: f64,
    pub checks: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailureIndexEntry {
    pub check: Option<String>,
    pub case: Option<String>,
    pub status: LeafStatus,
    pub exit: DiagnosticExit,
    pub termination_reason: Option<TerminationReason>,
    pub source: Option<SourceLocation>,
    pub error_category: ErrorCategory,
    pub expected: Option<DiagnosticValue>,
    pub actual: Option<DiagnosticValue>,
    pub fingerprint: String,
    pub occurrences: u32,
    pub log_refs: Vec<LogRef>,
    pub origin: DiagnosticOrigin,
}

impl FailureIndexEntry {
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(check) = &self.check {
            validate_name("diagnostic check", check, 64)?;
        }
        if let Some(case) = &self.case {
            if self.check.is_none() {
                return Err(ContractError::new(
                    "diagnostic case cannot be present without a check",
                ));
            }
            validate_diagnostic_name("diagnostic case", case)?;
        }
        if !matches!(
            self.status,
            LeafStatus::Failed
                | LeafStatus::TimedOut
                | LeafStatus::Invalidated
                | LeafStatus::NotMeaningful
                | LeafStatus::Cancelled
                | LeafStatus::Unsafe
        ) {
            return Err(ContractError::new(
                "failure diagnostics require a terminal non-success status",
            ));
        }
        self.exit.validate()?;
        if let Some(source) = &self.source {
            source.validate()?;
        }
        if let Some(expected) = &self.expected {
            expected.validate()?;
        }
        if let Some(actual) = &self.actual {
            actual.validate()?;
        }
        validate_fingerprint(&self.fingerprint)?;
        if self.occurrences == 0 {
            return Err(ContractError::new(
                "diagnostic occurrence count must be positive",
            ));
        }
        if self.log_refs.len() > MAX_DIAGNOSTIC_LOG_REFS {
            return Err(ContractError::new(format!(
                "diagnostic has more than {MAX_DIAGNOSTIC_LOG_REFS} log references"
            )));
        }
        let mut refs = BTreeSet::new();
        for log_ref in &self.log_refs {
            log_ref.validate()?;
            if !refs.insert(log_ref) {
                return Err(ContractError::new("diagnostic repeats a log reference"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReport {
    pub schema: Schema2,
    pub run_id: String,
    pub test: String,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub selection: Vec<String>,
    pub origin_run_id: Option<String>,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_seconds: f64,
    pub source_digest: String,
    pub config_digest: String,
    pub source_changed: bool,
    pub capacity: CapacityReport,
    pub counts: BTreeMap<String, u32>,
    pub checks: Vec<CheckReport>,
    #[serde(default)]
    pub phase_durations: Vec<PhaseDuration>,
    pub failure_index: Vec<FailureIndexEntry>,
    pub failure_index_truncated: bool,
}

impl ExecutionReport {
    pub fn from_json(input: &[u8]) -> Result<Self, ContractError> {
        if input.len() > MAX_REPORT_BYTES {
            return Err(ContractError::new("executor report exceeds 2 MiB"));
        }
        let report: Self = serde_json::from_slice(input)
            .map_err(|_| ContractError::new("executor report is not valid schema-2 JSON"))?;
        report.validate()?;
        Ok(report)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        validate_identity("report run_id", &self.run_id, 128)?;
        validate_name("report test", &self.test, 32)?;
        validate_digest("report source_digest", &self.source_digest)?;
        validate_digest("report config_digest", &self.config_digest)?;
        if !self.duration_seconds.is_finite() || self.duration_seconds < 0.0 {
            return Err(ContractError::new(
                "report duration_seconds must be finite and non-negative",
            ));
        }
        if self.readiness_eligible
            != (self.proof == ProofKind::Complete && self.requested_tier == ValidationTier::Release)
        {
            return Err(ContractError::new(
                "report readiness_eligible contradicts proof and tier",
            ));
        }
        match self.proof {
            ProofKind::Complete if !self.selection.is_empty() || self.origin_run_id.is_some() => {
                return Err(ContractError::new(
                    "complete report cannot include selection or origin_run_id",
                ));
            }
            ProofKind::Selected if self.selection.is_empty() || self.origin_run_id.is_some() => {
                return Err(ContractError::new(
                    "selected report requires selection and no origin_run_id",
                ));
            }
            ProofKind::Retry
                if self.selection.len() != 1
                    || self.origin_run_id.as_deref().is_none_or(str::is_empty) =>
            {
                return Err(ContractError::new(
                    "retry report requires one selection and an origin_run_id",
                ));
            }
            _ => {}
        }
        if let Some(origin) = &self.origin_run_id {
            validate_identity("report origin_run_id", origin, 128)?;
        }
        if self.selection.len() > 256
            || self.selection.iter().collect::<BTreeSet<_>>().len() != self.selection.len()
        {
            return Err(ContractError::new(
                "report selection is duplicated or exceeds 256 checks",
            ));
        }
        for check in &self.selection {
            validate_name("report selection", check, 64)?;
        }
        if self.status == RunStatus::Running && self.finished_at.is_some() {
            return Err(ContractError::new("running report cannot have finished_at"));
        }
        if self.status != RunStatus::Running && self.finished_at.is_none() {
            return Err(ContractError::new("terminal report requires finished_at"));
        }
        for value in [
            self.capacity.learned_capacity,
            self.capacity.effective_capacity,
        ] {
            if value == Some(0) {
                return Err(ContractError::new(
                    "report capacity must be null or positive",
                ));
            }
        }
        if self.checks.len() > 256 {
            return Err(ContractError::new("report exceeds 256 checks"));
        }
        let mut names = BTreeSet::new();
        let mut calculated = report_count_template();
        for check in &self.checks {
            validate_report_check(&self.run_id, check)?;
            if !names.insert(check.name.as_str()) {
                return Err(ContractError::new("report repeats a check"));
            }
            *calculated
                .get_mut(report_leaf_name(check.status))
                .expect("all leaf states are represented") += 1;
        }
        if self.counts != calculated {
            return Err(ContractError::new(
                "report counts do not match check states",
            ));
        }
        let mut phase_names = BTreeSet::new();
        for phase in &self.phase_durations {
            if !phase.duration_seconds.is_finite()
                || phase.duration_seconds < 0.0
                || phase.checks == 0
                || !phase_names.insert(phase.phase)
            {
                return Err(ContractError::new(
                    "report phase durations are invalid or duplicated",
                ));
            }
            let matching = self
                .checks
                .iter()
                .filter(|check| check.phase == phase.phase)
                .collect::<Vec<_>>();
            let total = matching
                .iter()
                .filter_map(|check| check.duration_seconds)
                .sum::<f64>();
            if usize::try_from(phase.checks).ok() != Some(matching.len())
                || (total - phase.duration_seconds).abs() > 0.001
            {
                return Err(ContractError::new(
                    "report phase duration does not match its independent checks",
                ));
            }
        }
        if self.failure_index.len() > MAX_FAILURE_INDEX {
            return Err(ContractError::new(format!(
                "report exceeds {MAX_FAILURE_INDEX} failure entries"
            )));
        }
        for failure in &self.failure_index {
            failure.validate()?;
            if failure
                .check
                .as_deref()
                .is_some_and(|check| !names.contains(check))
            {
                return Err(ContractError::new("failure entry names an unknown check"));
            }
            for log_ref in &failure.log_refs {
                if log_ref.run_id != self.run_id
                    || failure
                        .check
                        .as_ref()
                        .is_some_and(|check| log_ref.check.as_ref() != Some(check))
                {
                    return Err(ContractError::new(
                        "failure log reference belongs to another run or check",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_execution_progress(progress: &ExecutionProgress) -> Result<(), ContractError> {
    let total = u64::from(progress.waiting)
        + u64::from(progress.admitted)
        + u64::from(progress.executing)
        + u64::from(progress.finished);
    if total == 0
        || total > 4097
        || progress.queued_at_epoch_ms == 0
        || (progress.started_at_epoch_ms.is_some() && progress.admitted_at_epoch_ms.is_none())
        || (progress.executing > 0 && progress.started_at_epoch_ms.is_none())
        || (progress.admitted > 0 && progress.admitted_at_epoch_ms.is_none())
        || (progress.finished_at_epoch_ms.is_some()
            && progress.waiting + progress.admitted + progress.executing > 0)
    {
        return Err(ContractError::new(
            "process admission observation is inconsistent",
        ));
    }
    Ok(())
}

fn validate_report_check(run_id: &str, check: &CheckReport) -> Result<(), ContractError> {
    validate_name("report check", &check.name, 64)?;
    if !check.fingerprint.is_empty() {
        validate_digest("report check fingerprint", &check.fingerprint)?;
    }
    if check
        .duration_seconds
        .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err(ContractError::new(
            "check duration must be finite and non-negative",
        ));
    }
    if let Some(execution) = &check.execution {
        validate_execution_progress(execution)?;
    }
    check.exit.validate()?;
    if check.artifacts.len() > 16
        || check.cache_inputs.len() > 32
        || check.consumed_artifacts.len() > 16
        || check.retained_artifacts.len() > MAX_RETAINED_ARTIFACTS
    {
        return Err(ContractError::new("check report exceeds artifact bounds"));
    }
    let mut artifact_paths = BTreeSet::new();
    for artifact in &check.artifacts {
        validate_relative_path("report artifact", &artifact.path)?;
        validate_digest("report artifact sha256", &artifact.sha256)?;
        if !artifact_paths.insert(artifact.path.as_str()) {
            return Err(ContractError::new("check report repeats an artifact"));
        }
    }
    for (label, receipts) in [
        ("cache input", &check.cache_inputs),
        ("consumed artifact", &check.consumed_artifacts),
    ] {
        let mut paths = BTreeSet::new();
        for receipt in receipts {
            validate_relative_path(label, &receipt.path)?;
            validate_digest(&format!("{label} sha256"), &receipt.sha256)?;
            if !paths.insert(receipt.path.as_str()) {
                return Err(ContractError::new(format!(
                    "check report repeats a {label}"
                )));
            }
        }
    }
    let mut retained_names = BTreeSet::new();
    for artifact in &check.retained_artifacts {
        validate_name("report retained artifact", &artifact.name, 64)?;
        validate_digest("report retained artifact sha256", &artifact.sha256)?;
        if artifact.files == 0 || !retained_names.insert(artifact.name.as_str()) {
            return Err(ContractError::new(
                "retained artifact receipt is empty or duplicated",
            ));
        }
    }
    for stream in &check.streams {
        stream.validate()?;
        if stream.log_ref.run_id != run_id
            || stream.log_ref.check.as_deref() != Some(&check.name)
            || stream.log_ref.case.is_some()
        {
            return Err(ContractError::new(
                "check stream belongs to another execution leaf",
            ));
        }
    }
    if check.cases.len() > MAX_CASES
        || usize::try_from(check.case_count).unwrap_or(usize::MAX) < check.cases.len()
        || (!check.cases_truncated
            && usize::try_from(check.case_count).unwrap_or(usize::MAX) != check.cases.len())
    {
        return Err(ContractError::new(
            "check case count or truncation is inconsistent",
        ));
    }
    let mut cases = BTreeSet::new();
    for case in &check.cases {
        validate_case_id(&case.id)?;
        if !cases.insert(case.id.as_str()) {
            return Err(ContractError::new("check report repeats a case"));
        }
        if let Some(execution) = &case.execution {
            validate_execution_progress(execution)?;
        }
        case.exit.validate()?;
        for stream in &case.streams {
            stream.validate()?;
            if stream.log_ref.run_id != run_id
                || stream.log_ref.check.as_deref() != Some(&check.name)
                || stream.log_ref.case.as_deref() != Some(&case.id)
            {
                return Err(ContractError::new(
                    "case stream belongs to another execution leaf",
                ));
            }
        }
    }
    Ok(())
}

fn report_count_template() -> BTreeMap<String, u32> {
    [
        "pending",
        "running",
        "reused",
        "passed",
        "failed",
        "timed_out",
        "invalidated",
        "not_meaningful",
        "cancelled",
        "unsafe",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), 0))
    .collect()
}

fn report_leaf_name(status: LeafStatus) -> &'static str {
    match status {
        LeafStatus::Pending => "pending",
        LeafStatus::Running => "running",
        LeafStatus::Reused => "reused",
        LeafStatus::Passed => "passed",
        LeafStatus::Failed => "failed",
        LeafStatus::TimedOut => "timed_out",
        LeafStatus::Invalidated => "invalidated",
        LeafStatus::NotMeaningful => "not_meaningful",
        LeafStatus::Cancelled => "cancelled",
        LeafStatus::Unsafe => "unsafe",
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityReport {
    pub learned_capacity: Option<u32>,
    pub effective_capacity: Option<u32>,
    pub capacity_wait_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvent {
    pub schema: Schema2,
    pub run_id: String,
    pub check: String,
    pub status: EventStatus,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticEvent {
    pub schema: Schema2,
    pub run_id: String,
    pub check: String,
    pub case: Option<String>,
    pub status: LeafStatus,
    pub exit: DiagnosticExit,
    pub termination_reason: Option<TerminationReason>,
    pub source: Option<SourceLocation>,
    pub error_category: ErrorCategory,
    pub expected: Option<DiagnosticValue>,
    pub actual: Option<DiagnosticValue>,
    pub log_refs: Vec<LogRef>,
}

impl DiagnosticEvent {
    pub fn from_json(
        input: &[u8],
        expected_run_id: &str,
        expected_check: &str,
        expected_case: Option<&str>,
        expected_phase: LogPhase,
    ) -> Result<Self, ContractError> {
        if input.len() > MAX_EVENT_BYTES {
            return Err(ContractError::new("diagnostic event exceeds 4096 bytes"));
        }
        let event: Self = serde_json::from_slice(input)
            .map_err(|_| ContractError::new("diagnostic event is not valid schema-2 JSON"))?;
        event.validate(
            expected_run_id,
            expected_check,
            expected_case,
            expected_phase,
        )?;
        Ok(event)
    }

    pub fn validate(
        &self,
        expected_run_id: &str,
        expected_check: &str,
        expected_case: Option<&str>,
        expected_phase: LogPhase,
    ) -> Result<(), ContractError> {
        validate_identity("diagnostic run_id", &self.run_id, 128)?;
        validate_name("diagnostic check", &self.check, 64)?;
        if self.run_id != expected_run_id
            || self.check != expected_check
            || self.case.as_deref() != expected_case
        {
            return Err(ContractError::new(
                "diagnostic event identity does not match its execution leaf",
            ));
        }
        if let Some(case) = &self.case {
            validate_case_id(case)?;
        }
        if !matches!(
            self.status,
            LeafStatus::Failed | LeafStatus::TimedOut | LeafStatus::Cancelled | LeafStatus::Unsafe
        ) {
            return Err(ContractError::new(
                "diagnostic events require a failed, timed_out, cancelled, or unsafe status",
            ));
        }
        self.exit.validate()?;
        if let Some(source) = &self.source {
            source.validate()?;
        }
        if let Some(expected) = &self.expected {
            expected.validate()?;
        }
        if let Some(actual) = &self.actual {
            actual.validate()?;
        }
        if self.log_refs.len() > MAX_DIAGNOSTIC_LOG_REFS {
            return Err(ContractError::new(format!(
                "diagnostic event has more than {MAX_DIAGNOSTIC_LOG_REFS} log references"
            )));
        }
        let mut refs = BTreeSet::new();
        for log_ref in &self.log_refs {
            log_ref.validate()?;
            if log_ref.run_id != self.run_id || log_ref.check.as_deref() != Some(&self.check) {
                return Err(ContractError::new(
                    "diagnostic event log reference belongs to another leaf",
                ));
            }
            if log_ref.case != self.case {
                return Err(ContractError::new(
                    "diagnostic event case log reference belongs to another case",
                ));
            }
            if log_ref.phase != expected_phase {
                return Err(ContractError::new(
                    "diagnostic event log reference has the wrong leaf phase",
                ));
            }
            if !refs.insert(log_ref) {
                return Err(ContractError::new(
                    "diagnostic event repeats a log reference",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Passed,
    Failed,
    Unsafe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractError {
    message: String,
}

impl ContractError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, tier: ValidationTier) -> CheckPlan {
        CheckPlan {
            name: name.into(),
            tier,
            role: CheckRole::Work,
            phase: CheckPhase::Check,
            resources: Vec::new(),
            after: Vec::new(),
            requires: Vec::new(),
            invalidates: Vec::new(),
            cwd: ".".into(),
            env: BTreeMap::new(),
            timeout_seconds: Some(30),
            completion: CompletionMode::Process,
            on_failure: FailureMode::Continue,
            produces: Vec::new(),
            consumes: Vec::new(),
            cacheable: false,
            cache_inputs: Vec::new(),
            fingerprint: String::new(),
            expect_failure: false,
            qualification_of: None,
            retained_artifacts: Vec::new(),
            diagnostic_sources: Vec::new(),
            command: Some(vec!["true".into()]),
            discover: None,
            case_command: None,
            cases: None,
        }
    }

    fn plan(checks: Vec<CheckPlan>) -> ExecutionPlan {
        ExecutionPlan {
            schema: Schema2,
            run_id: "run-1".into(),
            test: "complete".into(),
            worktree_root: "/tmp/repo".into(),
            current_dir: "/tmp/repo/.devcoordinator/current".into(),
            log_dir: "/tmp/repo/.devcoordinator/test/logs/runs/run-1".into(),
            requested_tier: ValidationTier::Release,
            readiness_eligible: true,
            proof: ProofKind::Complete,
            selection: Vec::new(),
            origin_run_id: None,
            source_digest: "a".repeat(64),
            config_digest: "b".repeat(64),
            reused: BTreeMap::new(),
            reused_qualifications: BTreeSet::new(),
            case_selection: BTreeMap::new(),
            postgres_databases: BTreeMap::new(),
            checks,
        }
    }

    fn report() -> ExecutionReport {
        let check = CheckReport {
            execution: None,
            name: "unit".into(),
            tier: ValidationTier::Release,
            role: CheckRole::Work,
            phase: CheckPhase::Check,
            fingerprint: String::new(),
            cache_inputs: Vec::new(),
            consumed_artifacts: Vec::new(),
            status: LeafStatus::Passed,
            started_at: Some("2026-09-04T00:00:00Z".into()),
            finished_at: Some("2026-09-04T00:00:01Z".into()),
            duration_seconds: Some(1.0),
            exit: DiagnosticExit {
                code: Some(0),
                signal: None,
            },
            artifacts: Vec::new(),
            retained_artifacts: Vec::new(),
            streams: Vec::new(),
            case_count: 0,
            cases: Vec::new(),
            cases_truncated: false,
        };
        let mut counts = report_count_template();
        counts.insert("passed".into(), 1);
        ExecutionReport {
            schema: Schema2,
            run_id: "run-1".into(),
            test: "complete".into(),
            requested_tier: ValidationTier::Release,
            readiness_eligible: true,
            proof: ProofKind::Complete,
            selection: Vec::new(),
            origin_run_id: None,
            status: RunStatus::Passed,
            started_at: "2026-09-04T00:00:00Z".into(),
            finished_at: Some("2026-09-04T00:00:01Z".into()),
            duration_seconds: 1.0,
            source_digest: "a".repeat(64),
            config_digest: "b".repeat(64),
            source_changed: false,
            capacity: CapacityReport::default(),
            counts,
            checks: vec![check],
            phase_durations: vec![PhaseDuration {
                phase: CheckPhase::Check,
                duration_seconds: 1.0,
                checks: 1,
            }],
            failure_index: Vec::new(),
            failure_index_truncated: false,
        }
    }

    #[test]
    fn reports_are_strict_identity_bound_and_self_consistent() {
        let encoded = serde_json::to_vec(&report()).expect("report JSON");
        ExecutionReport::from_json(&encoded).expect("valid report");

        let mut inconsistent = report();
        inconsistent.counts.insert("passed".into(), 0);
        assert!(inconsistent.validate().is_err());

        let mut unknown = serde_json::to_value(report()).expect("report value");
        unknown
            .as_object_mut()
            .expect("object")
            .insert("raw_output".into(), serde_json::json!("private"));
        assert!(ExecutionReport::from_json(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }

    #[test]
    fn schema_one_is_rejected_without_fallback() {
        let json = serde_json::to_vec(&plan(vec![check("unit", ValidationTier::Development)]))
            .expect("serialize");
        let old =
            String::from_utf8(json)
                .expect("utf8")
                .replacen("\"schema\":2", "\"schema\":1", 1);
        let error = ExecutionPlan::from_json(old.as_bytes()).expect_err("schema one rejected");
        assert!(error.to_string().contains("schema must be 2"));
    }

    #[test]
    fn cycles_include_compiled_preflight_edges() {
        let mut preflight = check("preflight", ValidationTier::Development);
        preflight.role = CheckRole::Preflight;
        preflight.invalidates = vec!["work".into()];
        preflight.requires = vec!["work".into()];
        let work = check("work", ValidationTier::Development);
        let error = plan(vec![preflight, work])
            .validate()
            .expect_err("cycle rejected");
        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn lower_tier_cannot_depend_on_higher_tier() {
        let release = check("release", ValidationTier::Release);
        let mut development = check("development", ValidationTier::Development);
        development.requires.push("release".into());
        let error = plan(vec![release, development])
            .validate()
            .expect_err("tier inversion rejected");
        assert!(error.to_string().contains("higher-tier"));
    }

    #[test]
    fn retained_artifact_contract_is_bounded_direct_and_non_overlapping() {
        let mut direct = check("browser", ValidationTier::Development);
        direct.retained_artifacts = vec![
            RetainedArtifactSpec {
                name: "production".into(),
                path: "artifacts/production".into(),
                max_bytes: 512 * 1024 * 1024,
            },
            RetainedArtifactSpec {
                name: "developer-test".into(),
                path: "artifacts/developer-test".into(),
                max_bytes: 128 * 1024 * 1024,
            },
        ];
        plan(vec![direct.clone()])
            .validate()
            .expect("valid retained trees");

        let mut overlap = direct.clone();
        overlap.retained_artifacts[1].path = "artifacts/production/nested".into();
        assert!(
            plan(vec![overlap])
                .validate()
                .expect_err("overlap rejected")
                .to_string()
                .contains("overlap")
        );

        let mut reserved = direct.clone();
        reserved.retained_artifacts[0].path = ".devcoordinator/private".into();
        assert!(
            plan(vec![reserved])
                .validate()
                .expect_err("reserved path rejected")
                .to_string()
                .contains("reserved")
        );

        let mut fanout = direct;
        fanout.command = None;
        fanout.case_command = Some(vec!["true".into()]);
        fanout.cases = Some(vec![CaseSpec {
            id: "one".into(),
            args: Vec::new(),
            postgres: None,
        }]);
        assert!(
            plan(vec![fanout])
                .validate()
                .expect_err("fanout retained tree rejected")
                .to_string()
                .contains("fan-out")
        );
    }

    #[test]
    fn nested_or_ambiguous_fanout_is_rejected() {
        let mut invalid = check("fanout", ValidationTier::Development);
        invalid.command = Some(vec!["true".into()]);
        invalid.discover = Some(vec!["discover".into()]);
        invalid.case_command = Some(vec!["case".into()]);
        let error = plan(vec![invalid])
            .validate()
            .expect_err("ambiguous fanout rejected");
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn compiled_invalidation_requires_pair_is_canonical() {
        let mut preflight = check("preflight", ValidationTier::Development);
        preflight.role = CheckRole::Preflight;
        preflight.invalidates = vec!["work".into()];
        let mut work = check("work", ValidationTier::Development);
        work.requires = vec!["preflight".into()];
        plan(vec![preflight, work])
            .validate()
            .expect("compiled invalidation edge accepted");
    }

    #[test]
    fn malformed_and_oversized_manifests_are_rejected() {
        assert!(CaseManifest::from_json(br#"{"schema":2,"cases":"bad"}"#).is_err());
        let oversized = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert!(CaseManifest::from_json(&oversized).is_err());
    }

    #[test]
    fn duplicate_case_ids_are_rejected() {
        let manifest = CaseManifest {
            schema: Schema2,
            cases: vec![
                CaseSpec {
                    id: "same".into(),
                    args: Vec::new(),
                    postgres: None,
                },
                CaseSpec {
                    id: "same".into(),
                    args: Vec::new(),
                    postgres: None,
                },
            ],
        };
        assert!(validate_cases(&manifest.cases).is_err());
    }

    #[test]
    fn log_directory_is_required_and_must_remain_below_the_worktree() {
        let valid = plan(vec![check("unit", ValidationTier::Development)]);
        valid.validate().expect("contained log directory accepted");

        let json = serde_json::to_value(&valid).expect("serialize plan");
        let mut missing = json.clone();
        missing.as_object_mut().expect("object").remove("log_dir");
        let encoded = serde_json::to_vec(&missing).expect("encode missing plan");
        assert!(ExecutionPlan::from_json(&encoded).is_err());

        let mut escaped = valid.clone();
        escaped.log_dir = "/tmp/repo/../outside".into();
        assert!(escaped.validate().is_err());

        let mut normalized = valid;
        normalized.log_dir =
            "/tmp/repo/.devcoordinator/../.devcoordinator/test/logs/runs/run-1".into();
        normalized
            .validate()
            .expect("lexically normalized contained path accepted");
    }

    #[test]
    fn run_identities_are_safe_path_components_in_every_contract() {
        for invalid in ["..", ".hidden", "-leading", "run:one", "run/one"] {
            let mut candidate = plan(vec![check("unit", ValidationTier::Development)]);
            candidate.run_id = invalid.into();
            assert!(candidate.validate().is_err(), "accepted {invalid:?}");
        }
        let mut valid = plan(vec![check("unit", ValidationTier::Development)]);
        valid.run_id = "skills-20260902T120000Z-123-abcdef".into();
        valid.log_dir = format!("/tmp/repo/.devcoordinator/test/logs/runs/{}", valid.run_id);
        valid.validate().expect("generic safe run identity");
    }

    #[test]
    fn diagnostic_sources_are_strict_bounded_and_relative() {
        let mut unit = check("unit", ValidationTier::Development);
        unit.diagnostic_sources = vec![DiagnosticReportSource {
            format: DiagnosticReportFormat::Junit,
            path: "junit/results.xml".into(),
        }];
        plan(vec![unit.clone()])
            .validate()
            .expect("one relative source accepted");

        unit.diagnostic_sources
            .push(unit.diagnostic_sources[0].clone());
        assert!(plan(vec![unit]).validate().is_err());

        let mut escaped = check("unit", ValidationTier::Development);
        escaped.diagnostic_sources = vec![DiagnosticReportSource {
            format: DiagnosticReportFormat::PlaywrightJson,
            path: "../report.json".into(),
        }];
        assert!(plan(vec![escaped]).validate().is_err());
    }

    fn log_ref(case: Option<&str>) -> LogRef {
        LogRef {
            run_id: "run-1".into(),
            check: Some("unit".into()),
            phase: if case.is_some() {
                LogPhase::Case
            } else {
                LogPhase::Check
            },
            case: case.map(str::to_owned),
            stream: LogStream::Stderr,
        }
    }

    #[test]
    fn log_references_enforce_phase_identity() {
        log_ref(Some("parser-17"))
            .validate()
            .expect("complete case selector accepted");
        let mut missing_case = log_ref(Some("parser-17"));
        missing_case.case = None;
        assert!(missing_case.validate().is_err());
        let executor = LogRef {
            run_id: "run-1".into(),
            check: Some("unit".into()),
            phase: LogPhase::Executor,
            case: None,
            stream: LogStream::Stdout,
        };
        assert!(executor.validate().is_err());
    }

    #[test]
    fn stream_summaries_are_exact_and_have_no_legacy_truncation_projection() {
        let summary = LogStreamSummary {
            log_ref: log_ref(None),
            bytes: 0,
            lines: 0,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            first_write_epoch_ms: None,
            last_write_epoch_ms: None,
            complete: true,
        };
        summary.validate().expect("empty complete stream");
        let encoded = serde_json::to_string(&summary).expect("stream JSON");
        assert!(!encoded.contains("retained"));
        assert!(!encoded.contains("observed"));
        assert!(!encoded.contains("truncated"));

        let mut invalid = summary;
        invalid.bytes = 1;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn diagnostic_event_is_identity_bound_and_rejects_raw_prose() {
        let event = serde_json::json!({
            "schema": 2,
            "run_id": "run-1",
            "check": "unit",
            "case": "parser-17",
            "status": "failed",
            "exit": {"code": 1, "signal": null},
            "termination_reason": null,
            "source": {"file": "src/parser.rs", "line": 81, "column": 9},
            "error_category": "assertion",
            "expected": null,
            "actual": null,
            "log_refs": [log_ref(Some("parser-17"))]
        });
        let encoded = serde_json::to_vec(&event).expect("event JSON");
        DiagnosticEvent::from_json(&encoded, "run-1", "unit", Some("parser-17"), LogPhase::Case)
            .expect("matching event accepted");
        assert!(
            DiagnosticEvent::from_json(
                &encoded,
                "other",
                "unit",
                Some("parser-17"),
                LogPhase::Case,
            )
            .is_err()
        );

        assert!(
            DiagnosticEvent::from_json(
                &encoded,
                "run-1",
                "unit",
                Some("parser-17"),
                LogPhase::Check,
            )
            .is_err()
        );

        let mut with_message = event;
        with_message
            .as_object_mut()
            .expect("object")
            .insert("message".into(), serde_json::json!("raw stack trace"));
        let encoded = serde_json::to_vec(&with_message).expect("event JSON");
        assert!(
            DiagnosticEvent::from_json(
                &encoded,
                "run-1",
                "unit",
                Some("parser-17"),
                LogPhase::Case,
            )
            .is_err()
        );
    }

    #[test]
    fn diagnostic_values_and_sources_are_bounded() {
        let value = DiagnosticValue {
            value_type: DiagnosticValueType::String,
            preview: Some("ready".into()),
            sha256: "a".repeat(64),
            byte_count: 5,
            truncated: false,
            redacted: false,
        };
        value.validate().expect("bounded value accepted");
        let mut multiline = value;
        multiline.preview = Some("first\nsecond".into());
        assert!(multiline.validate().is_err());

        SourceLocation {
            file: "src/parser.rs".into(),
            line: 1,
            column: None,
        }
        .validate()
        .expect("relative source accepted");
        assert!(
            SourceLocation {
                file: "/etc/passwd".into(),
                line: 1,
                column: None,
            }
            .validate()
            .is_err()
        );
    }
}
