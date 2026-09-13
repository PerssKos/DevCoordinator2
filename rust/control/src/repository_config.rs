//! Strict `.devcoordinator.toml` schema-2 test and deployment configuration.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use devcoordinator2_executor_protocol::{
    CaseSpec, CheckPlan, CheckRole, CompletionMode, DiagnosticReportFormat, DiagnosticReportSource,
    FailureMode, RetainedArtifactSpec, ValidationTier,
};
use regex::Regex;
use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml::{Table, Value};

pub const CONFIG_NAME: &str = ".devcoordinator.toml";
pub const MAX_CONFIG_BYTES: u64 = 262_144;
pub const TIMEOUT_MIN: u64 = 1;
pub const TIMEOUT_MAX: u64 = 21_600;
pub const TIMEOUT_DEFAULT: u64 = 600;
pub const MAX_DIAGNOSTIC_SOURCES: usize = 8;
pub const MAX_RETAINED_ARTIFACTS: usize = 8;
pub const MAX_RETAINED_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_RETAINED_ARTIFACT_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const POSTGRES_IMAGE_DEFAULT: &str = "postgres:16-alpine";
pub const HEALTH_TIMEOUT_DEFAULT: u64 = 60;
pub const COMPOSE_READINESS_TIMEOUT_DEFAULT: u64 = 300;

#[derive(Debug, Error, Clone, Eq, PartialEq)]
#[error("{message}")]
pub struct RepositoryConfigError {
    pub message: String,
}

impl RepositoryConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostgresSpec {
    pub image: String,
    pub database: String,
    pub user: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestSpec {
    pub name: String,
    pub cwd: PathBuf,
    pub timeout_seconds: u64,
    pub env: BTreeMap<String, String>,
    pub postgres: Option<PostgresSpec>,
    pub checks: Vec<CheckPlan>,
    pub config_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestConfigSummary {
    pub name: String,
    pub tiers: Vec<ValidationTier>,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthKind {
    Http,
    Tcp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HealthSpec {
    pub kind: HealthKind,
    pub path: Option<String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Process,
    Docker,
    Compose,
    Postgres,
    External,
}

impl ComponentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Docker => "docker",
            Self::Compose => "compose",
            Self::Postgres => "postgres",
            Self::External => "external",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ComponentSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ComponentKind,
    pub order: usize,
    pub independent_control: bool,
    pub depends_on: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub cwd: String,
    pub wants_port: bool,
    pub route: bool,
    pub health: Option<HealthSpec>,
    pub persistent_paths: Vec<String>,
    pub image: Option<String>,
    pub container_port: Option<u16>,
    pub volumes: Vec<String>,
    pub compose_files: Vec<String>,
    pub compose_env_file: Option<String>,
    pub compose_build: bool,
    pub services: Vec<String>,
    pub finite_services: Vec<String>,
    pub independent_services: Vec<String>,
    pub compose_timeout_seconds: u64,
    pub database: Option<String>,
    pub user: Option<String>,
    pub shared_from: Option<String>,
    pub tcp: Option<String>,
}

impl ComponentSpec {
    pub fn is_finite_workload(&self) -> bool {
        self.kind == ComponentKind::Compose
            && !self.services.is_empty()
            && self.services.len() == self.finite_services.len()
            && self
                .services
                .iter()
                .all(|service| self.finite_services.contains(service))
    }

    pub fn owns_persistent_data(&self) -> bool {
        (self.kind == ComponentKind::Postgres && self.shared_from.is_none())
            || !self.volumes.is_empty()
            || !self.persistent_paths.is_empty()
            || self.kind == ComponentKind::Compose
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentSpec {
    pub name: String,
    pub sources: Vec<String>,
    pub domains: BTreeMap<String, String>,
    pub build: Vec<String>,
    pub ttl_seconds: Option<u64>,
    pub public: bool,
    pub components: Vec<ComponentSpec>,
}

impl DeploymentSpec {
    pub fn domain_for(&self, source: &str) -> Option<&str> {
        self.domains.get(source).map(String::as_str)
    }

    pub fn component(&self, name: &str) -> Option<&ComponentSpec> {
        self.components
            .iter()
            .find(|component| component.name == name)
    }

    pub fn route_component(&self) -> Option<&ComponentSpec> {
        self.components
            .iter()
            .find(|component| component.route)
            .or_else(|| {
                let candidates = self
                    .components
                    .iter()
                    .filter(|component| {
                        component.wants_port
                            && matches!(
                                component.kind,
                                ComponentKind::Process
                                    | ComponentKind::Docker
                                    | ComponentKind::Compose
                            )
                    })
                    .collect::<Vec<_>>();
                (candidates.len() == 1).then(|| candidates[0])
            })
    }

    pub fn canonical(&self, source: &str) -> JsonValue {
        json!({
            "name": self.name,
            "source": source,
            "domain": self.domain_for(source),
            "build": self.build,
            "ttl_seconds": self.ttl_seconds,
            "public": self.public,
            "components": self.components,
        })
    }

    pub fn fingerprint(&self, source: &str) -> String {
        lower_hex(&Sha256::digest(
            serde_json::to_vec(&self.canonical(source)).expect("deployment spec is serializable"),
        ))[..24]
            .to_owned()
    }
}

struct ParsedConfig {
    root: PathBuf,
    table: Table,
    digest: String,
}

pub fn validate_test_config(
    worktree_root: &Path,
) -> Result<Vec<TestConfigSummary>, RepositoryConfigError> {
    let (default, specs) = load_all_test_specs(worktree_root)?;
    Ok(specs
        .into_iter()
        .map(|(name, specification)| TestConfigSummary {
            tiers: [
                ValidationTier::Development,
                ValidationTier::PreMerge,
                ValidationTier::Release,
            ]
            .into_iter()
            .filter(|tier| specification.checks.iter().any(|check| check.tier == *tier))
            .collect(),
            default: default.as_deref() == Some(&name),
            name,
        })
        .collect())
}

pub fn load_test_spec(
    worktree_root: &Path,
    test_name: Option<&str>,
) -> Result<TestSpec, RepositoryConfigError> {
    let (default, mut named) = load_all_test_specs(worktree_root)?;
    let selected = match test_name {
        Some(name) => name.to_owned(),
        None => match default {
            Some(default) => default,
            None if named.len() == 1 => named.keys().next().expect("one test").clone(),
            None => {
                return Err(RepositoryConfigError::new(
                    "multiple tests defined; set test.default or name one",
                ));
            }
        },
    };
    named.remove(&selected).ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "test {selected:?} is not defined (available: {:?})",
            named.keys().collect::<Vec<_>>()
        ))
    })
}

pub fn list_deployment_names(worktree_root: &Path) -> Result<Vec<String>, RepositoryConfigError> {
    let parsed = read_config(worktree_root)?;
    let Some(deployment) = parsed.table.get("deployment") else {
        return Ok(Vec::new());
    };
    let deployment = deployment
        .as_table()
        .ok_or_else(|| RepositoryConfigError::new("[deployment] must contain named tables"))?;
    let mut names = deployment.keys().cloned().collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

pub fn load_deployment_spec(
    worktree_root: &Path,
    name: &str,
) -> Result<DeploymentSpec, RepositoryConfigError> {
    let parsed = read_config(worktree_root)?;
    let mut available = parsed
        .table
        .get("deployment")
        .and_then(Value::as_table)
        .map(|table| table.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    available.sort();
    let deployment = parsed
        .table
        .get("deployment")
        .and_then(Value::as_table)
        .ok_or_else(|| {
            RepositoryConfigError::new(format!(
                "deployment {name:?} is not defined (available: {available:?})"
            ))
        })?;
    let value = deployment.get(name).ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "deployment {name:?} is not defined (available: {available:?})"
        ))
    })?;
    let body = value.as_table().ok_or_else(|| {
        RepositoryConfigError::new(format!("[deployment.{name}] must be a table"))
    })?;
    if !name_regex().is_match(name) {
        return Err(RepositoryConfigError::new(format!(
            "invalid deployment name {name:?}"
        )));
    }
    validate_deployment(&parsed.root, name, body)
}

fn read_config(worktree_root: &Path) -> Result<ParsedConfig, RepositoryConfigError> {
    let root = worktree_root.canonicalize().map_err(|_| {
        RepositoryConfigError::new("worktree root is unavailable or not a real directory")
    })?;
    let directory = open(
        &root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| RepositoryConfigError::new("worktree root is unavailable"))?;
    let file = openat(
        &directory,
        CONFIG_NAME,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            RepositoryConfigError::new(format!("{CONFIG_NAME} not found"))
        } else {
            RepositoryConfigError::new(format!("cannot read {CONFIG_NAME}"))
        }
    })?;
    let metadata = fstat(&file)
        .map_err(|_| RepositoryConfigError::new(format!("cannot inspect {CONFIG_NAME}")))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(RepositoryConfigError::new(format!(
            "{CONFIG_NAME} must be a regular non-symlink file"
        )));
    }
    if metadata.st_size < 0 || metadata.st_size as u64 > MAX_CONFIG_BYTES {
        return Err(RepositoryConfigError::new(format!(
            "{CONFIG_NAME} exceeds {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let mut raw = Vec::with_capacity(metadata.st_size as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| RepositoryConfigError::new(format!("cannot read {CONFIG_NAME}")))?;
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        return Err(RepositoryConfigError::new(format!(
            "{CONFIG_NAME} exceeds {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| RepositoryConfigError::new("invalid TOML: input is not UTF-8"))?;
    let data: Value = toml::from_str(text)
        .map_err(|error| RepositoryConfigError::new(format!("invalid TOML: {error}")))?;
    let table = data
        .as_table()
        .cloned()
        .ok_or_else(|| RepositoryConfigError::new("configuration must be a TOML table"))?;
    reject_unknown(&table, &["schema", "test", "deployment"], "top-level")?;
    if table.get("schema").and_then(Value::as_integer) != Some(2) {
        return Err(RepositoryConfigError::new(
            "'schema' must be 2; schema 1 is no longer supported",
        ));
    }
    Ok(ParsedConfig {
        root,
        table,
        digest: lower_hex(&Sha256::digest(raw)),
    })
}

fn load_all_test_specs(
    worktree_root: &Path,
) -> Result<(Option<String>, BTreeMap<String, TestSpec>), RepositoryConfigError> {
    let parsed = read_config(worktree_root)?;
    let tests = parsed
        .table
        .get("test")
        .and_then(Value::as_table)
        .ok_or_else(|| RepositoryConfigError::new("a [test.<name>] section is required"))?;
    let default = match tests.get("default") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| {
                    RepositoryConfigError::new("test.default must name a configured test")
                })?
                .to_owned(),
        ),
        None => None,
    };
    let mut named = BTreeMap::new();
    for (name, value) in tests {
        if name == "default" {
            continue;
        }
        if !test_name_regex().is_match(name) {
            return Err(RepositoryConfigError::new(format!(
                "invalid test name {name:?}"
            )));
        }
        let section = value
            .as_table()
            .ok_or_else(|| RepositoryConfigError::new(format!("[test.{name}] must be a table")))?;
        named.insert(
            name.clone(),
            validate_test(&parsed.root, name, section, &parsed.digest)?,
        );
    }
    if named.is_empty() {
        return Err(RepositoryConfigError::new(
            "at least one [test.<name>] section is required",
        ));
    }
    if let Some(default) = &default
        && !named.contains_key(default)
    {
        return Err(RepositoryConfigError::new(format!(
            "test.default {default:?} is not defined"
        )));
    }
    Ok((default, named))
}

fn validate_test(
    root: &Path,
    name: &str,
    section: &Table,
    digest: &str,
) -> Result<TestSpec, RepositoryConfigError> {
    if section.contains_key("command") || section.contains_key("tier") {
        return Err(RepositoryConfigError::new(format!(
            "[test.{name}] schema 2 rejects direct test commands; declare one or more [[test.{name}.check]] tables"
        )));
    }
    reject_unknown(
        section,
        &["cwd", "timeout_seconds", "env", "postgres", "check"],
        &format!("[test.{name}]"),
    )?;
    let checks = section.get("check").ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "[test.{name}] requires at least one [[test.{name}.check]] table"
        ))
    })?;
    let cwd = validate_cwd(
        root,
        &format!("[test.{name}].cwd"),
        section.get("cwd").and_then(Value::as_str).unwrap_or("."),
    )?;
    if section.get("cwd").is_some_and(|value| !value.is_str()) {
        return Err(RepositoryConfigError::new(format!(
            "[test.{name}].cwd must be a string"
        )));
    }
    let timeout_seconds = bounded_integer(
        section.get("timeout_seconds"),
        TIMEOUT_DEFAULT,
        TIMEOUT_MIN,
        TIMEOUT_MAX,
        &format!("[test.{name}] timeout_seconds"),
    )?;
    let env = validate_env(&format!("[test.{name}].env"), section.get("env"), true)?;
    let postgres = section
        .get("postgres")
        .map(|value| validate_test_postgres(name, value))
        .transpose()?;
    let checks = validate_checks(root, name, &cwd, checks)?;
    Ok(TestSpec {
        name: name.to_owned(),
        cwd,
        timeout_seconds,
        env,
        postgres,
        checks,
        config_digest: digest.to_owned(),
    })
}

fn validate_checks(
    root: &Path,
    test_name: &str,
    default_cwd: &Path,
    value: &Value,
) -> Result<Vec<CheckPlan>, RepositoryConfigError> {
    let raw = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "[test.{test_name}].check must be a non-empty array"
        ))
    })?;
    if raw.is_empty() || raw.len() > 256 {
        return Err(RepositoryConfigError::new(format!(
            "[test.{test_name}].check must contain 1..256 checks"
        )));
    }
    let allowed = [
        "name",
        "tier",
        "role",
        "command",
        "discover",
        "case_command",
        "cases",
        "cwd",
        "env",
        "after",
        "requires",
        "completion",
        "on_failure",
        "produces",
        "timeout_seconds",
        "invalidates",
        "diagnostic_sources",
        "retained_artifacts",
    ];
    let mut checks = Vec::with_capacity(raw.len());
    let mut seen = BTreeSet::new();
    for (index, value) in raw.iter().enumerate() {
        let label = format!("[test.{test_name}.check[{index}]]");
        let item = value
            .as_table()
            .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a table")))?;
        reject_unknown(item, &allowed, &label)?;
        let name = required_string(item, "name", &format!("{label}.name"))?;
        if !check_name_regex().is_match(&name) {
            return Err(RepositoryConfigError::new(format!(
                "{label}.name is invalid"
            )));
        }
        if !seen.insert(name.clone()) {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}] duplicate check {name:?}"
            )));
        }
        let tier = parse_tier(
            item.get("tier")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    RepositoryConfigError::new(format!(
                        "{label}.tier must be 'development', 'pre-merge', or 'release'"
                    ))
                })?
                .to_owned(),
            &label,
        )?;
        let role = match item.get("role").and_then(Value::as_str).unwrap_or("work") {
            "work" => CheckRole::Work,
            "preflight" => CheckRole::Preflight,
            _ => {
                return Err(RepositoryConfigError::new(format!(
                    "{label}.role must be 'work' or 'preflight'"
                )));
            }
        };
        if item.get("role").is_some_and(|value| !value.is_str()) {
            return Err(RepositoryConfigError::new(format!(
                "{label}.role must be 'work' or 'preflight'"
            )));
        }
        let forms = ["command", "discover", "cases"]
            .into_iter()
            .filter(|key| item.contains_key(*key))
            .count();
        if forms != 1 {
            return Err(RepositoryConfigError::new(format!(
                "{label} requires exactly one of command, discover, or cases"
            )));
        }
        let command = item
            .get("command")
            .map(|value| validate_command(&format!("{label}.command"), value))
            .transpose()?;
        let discover = item
            .get("discover")
            .map(|value| validate_command(&format!("{label}.discover"), value))
            .transpose()?;
        let fanout = discover.is_some() || item.contains_key("cases");
        let case_command = if fanout {
            Some(validate_command(
                &format!("{label}.case_command"),
                item.get("case_command").ok_or_else(|| {
                    RepositoryConfigError::new(format!(
                        "{label}.case_command must contain an argv array"
                    ))
                })?,
            )?)
        } else {
            if item.contains_key("case_command") {
                return Err(RepositoryConfigError::new(format!(
                    "{label}.case_command requires discover or cases"
                )));
            }
            None
        };
        let cases = item
            .get("cases")
            .map(|value| validate_cases(&format!("{label}.cases"), value))
            .transpose()?;
        let cwd = match item.get("cwd") {
            Some(value) => validate_cwd(
                root,
                &format!("{label}.cwd"),
                value.as_str().ok_or_else(|| {
                    RepositoryConfigError::new(format!("{label}.cwd must be a string"))
                })?,
            )?,
            None => default_cwd.to_owned(),
        };
        let after = string_list(&format!("{label}.after"), item.get("after"), false)?;
        let requires = string_list(&format!("{label}.requires"), item.get("requires"), false)?;
        if after.iter().any(|dependency| requires.contains(dependency)) {
            return Err(RepositoryConfigError::new(format!(
                "{label} repeats a dependency in after and requires"
            )));
        }
        let completion = match item
            .get("completion")
            .and_then(Value::as_str)
            .unwrap_or("process")
        {
            "process" => CompletionMode::Process,
            "event" => CompletionMode::Event,
            _ => {
                return Err(RepositoryConfigError::new(format!(
                    "{label}.completion must be 'process' or 'event'"
                )));
            }
        };
        if item.get("completion").is_some_and(|value| !value.is_str()) {
            return Err(RepositoryConfigError::new(format!(
                "{label}.completion must be 'process' or 'event'"
            )));
        }
        if completion == CompletionMode::Event
            && (role == CheckRole::Preflight || command.is_none())
        {
            return Err(RepositoryConfigError::new(format!(
                "{label}.completion 'event' is unavailable for preflights or fan-out"
            )));
        }
        let on_failure = match item
            .get("on_failure")
            .and_then(Value::as_str)
            .unwrap_or("continue")
        {
            "continue" => FailureMode::Continue,
            "stop" => FailureMode::Stop,
            _ => {
                return Err(RepositoryConfigError::new(format!(
                    "{label}.on_failure must be 'continue' or 'stop'"
                )));
            }
        };
        if item.get("on_failure").is_some_and(|value| !value.is_str()) {
            return Err(RepositoryConfigError::new(format!(
                "{label}.on_failure must be 'continue' or 'stop'"
            )));
        }
        let produces = string_list(&format!("{label}.produces"), item.get("produces"), false)?;
        if produces.len() > 16 {
            return Err(RepositoryConfigError::new(format!(
                "{label}.produces exceeds 16 paths"
            )));
        }
        let produces = produces
            .into_iter()
            .map(|path| validate_artifact_path(&format!("{label}.produces"), &path))
            .collect::<Result<Vec<_>, _>>()?;
        let retained_artifacts = validate_retained_artifacts(
            &format!("{label}.retained_artifacts"),
            item.get("retained_artifacts"),
        )?;
        if !retained_artifacts.is_empty() && command.is_none() {
            return Err(RepositoryConfigError::new(format!(
                "{label}.retained_artifacts is available only for direct checks"
            )));
        }
        if !retained_artifacts.is_empty() && completion != CompletionMode::Process {
            return Err(RepositoryConfigError::new(format!(
                "{label}.retained_artifacts requires process completion"
            )));
        }
        let diagnostic_sources = validate_diagnostic_sources(
            &format!("{label}.diagnostic_sources"),
            item.get("diagnostic_sources"),
        )?;
        let timeout_seconds = item
            .get("timeout_seconds")
            .map(|_| {
                bounded_integer(
                    item.get("timeout_seconds"),
                    TIMEOUT_DEFAULT,
                    TIMEOUT_MIN,
                    TIMEOUT_MAX,
                    &format!("{label}.timeout_seconds"),
                )
            })
            .transpose()?;
        let invalidates = string_list(
            &format!("{label}.invalidates"),
            item.get("invalidates"),
            false,
        )?;
        if !invalidates.is_empty() && role != CheckRole::Preflight {
            return Err(RepositoryConfigError::new(format!(
                "{label}.invalidates requires role = 'preflight'"
            )));
        }
        if role == CheckRole::Preflight && invalidates.is_empty() {
            return Err(RepositoryConfigError::new(format!(
                "{label} preflight must invalidate at least one check"
            )));
        }
        checks.push(CheckPlan {
            name,
            tier,
            role,
            phase: devcoordinator2_executor_protocol::CheckPhase::Check,
            resources: Vec::new(),
            consumes: Vec::new(),
            cacheable: false,
            cache_inputs: Vec::new(),
            fingerprint: String::new(),
            expect_failure: false,
            qualification_of: None,
            after,
            requires,
            invalidates,
            cwd: cwd.to_string_lossy().into_owned(),
            env: validate_env(&format!("{label}.env"), item.get("env"), true)?,
            timeout_seconds,
            completion,
            on_failure,
            produces,
            retained_artifacts,
            diagnostic_sources,
            command,
            discover,
            case_command,
            cases,
        });
    }
    validate_and_compile_dependencies(test_name, checks)
}

fn validate_and_compile_dependencies(
    test_name: &str,
    mut checks: Vec<CheckPlan>,
) -> Result<Vec<CheckPlan>, RepositoryConfigError> {
    let names = checks
        .iter()
        .map(|check| check.name.clone())
        .collect::<HashSet<_>>();
    let tiers = checks
        .iter()
        .map(|check| (check.name.clone(), check.tier))
        .collect::<HashMap<_, _>>();
    for check in &checks {
        let missing = check
            .after
            .iter()
            .chain(&check.requires)
            .chain(&check.invalidates)
            .filter(|name| !names.contains(*name))
            .cloned()
            .collect::<BTreeSet<_>>();
        if !missing.is_empty() {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}.check.{}] unknown dependencies: {missing:?}",
                check.name
            )));
        }
        if check.after.contains(&check.name) || check.requires.contains(&check.name) {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}.check.{}] cannot depend on itself",
                check.name
            )));
        }
        if check.invalidates.contains(&check.name) {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}.check.{}] cannot invalidate itself",
                check.name
            )));
        }
        for dependency in check.after.iter().chain(&check.requires) {
            if tier_rank(tiers[dependency]) > tier_rank(check.tier) {
                return Err(RepositoryConfigError::new(format!(
                    "[test.{test_name}.check.{}] tier inversion: dependency {dependency:?} belongs to a higher tier",
                    check.name
                )));
            }
        }
    }
    let indexes = checks
        .iter()
        .enumerate()
        .map(|(index, check)| (check.name.clone(), index))
        .collect::<HashMap<_, _>>();
    let invalidations = checks
        .iter()
        .flat_map(|check| {
            check
                .invalidates
                .iter()
                .map(|target| (check.name.clone(), check.tier, target.clone()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (preflight, tier, target_name) in invalidations {
        let target = &mut checks[indexes[&target_name]];
        if tier_rank(tier) > tier_rank(target.tier) {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}.check.{preflight}] tier inversion: cannot invalidate lower-tier check {target_name:?}"
            )));
        }
        if target.after.contains(&preflight) || target.requires.contains(&preflight) {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}] duplicate dependency edge from {preflight:?} to {target_name:?}"
            )));
        }
        target.requires.push(preflight);
    }
    let edges = checks
        .iter()
        .map(|check| {
            (
                check.name.clone(),
                check
                    .after
                    .iter()
                    .chain(&check.requires)
                    .cloned()
                    .collect::<HashSet<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut remaining = names;
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .filter(|name| edges[*name].is_disjoint(&remaining))
            .cloned()
            .collect::<Vec<_>>();
        if ready.is_empty() {
            return Err(RepositoryConfigError::new(format!(
                "[test.{test_name}] check dependencies contain a cycle"
            )));
        }
        for name in ready {
            remaining.remove(&name);
        }
    }
    Ok(checks)
}

fn validate_test_postgres(
    test_name: &str,
    value: &Value,
) -> Result<PostgresSpec, RepositoryConfigError> {
    let label = format!("[test.{test_name}.postgres]");
    let table = value
        .as_table()
        .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a table")))?;
    reject_unknown(table, &["image", "database", "user"], &label)?;
    let image = optional_string(table, "image", &format!("{label}.image"))?
        .unwrap_or_else(|| POSTGRES_IMAGE_DEFAULT.to_owned());
    if !postgres_image_regex().is_match(&image) && !postgres_digest_image_regex().is_match(&image) {
        return Err(RepositoryConfigError::new(format!(
            "{label} image must be an official 'postgres:<tag>' reference or a PostgreSQL-compatible image pinned by sha256 digest"
        )));
    }
    let database = optional_string(table, "database", &format!("{label}.database"))?
        .unwrap_or_else(|| "test".into());
    let user =
        optional_string(table, "user", &format!("{label}.user"))?.unwrap_or_else(|| "test".into());
    for (field, value) in [("database", &database), ("user", &user)] {
        if !postgres_ident_regex().is_match(value) {
            return Err(RepositoryConfigError::new(format!(
                "{label} {field} must match [a-z_][a-z0-9_]{{0,62}}"
            )));
        }
    }
    Ok(PostgresSpec {
        image,
        database,
        user,
    })
}

fn validate_command(label: &str, value: &Value) -> Result<Vec<String>, RepositoryConfigError> {
    let array = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "{label} must be an argv array, never a shell string"
        ))
    })?;
    if array.is_empty() || array.len() > 256 {
        return Err(RepositoryConfigError::new(format!(
            "{label} must contain 1..256 non-empty strings of at most 4096 bytes"
        )));
    }
    array
        .iter()
        .map(|argument| {
            argument
                .as_str()
                .filter(|argument| !argument.is_empty() && argument.len() <= 4096)
                .map(str::to_owned)
                .ok_or_else(|| {
                    RepositoryConfigError::new(format!(
                        "{label} must contain 1..256 non-empty strings of at most 4096 bytes"
                    ))
                })
        })
        .collect()
}

fn validate_cases(label: &str, value: &Value) -> Result<Vec<CaseSpec>, RepositoryConfigError> {
    let array = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!("{label} must be a non-empty array of case tables"))
    })?;
    if array.is_empty() || array.len() > 4096 {
        return Err(RepositoryConfigError::new(format!(
            "{label} must contain 1..4096 case tables"
        )));
    }
    let mut seen = HashSet::new();
    let mut cases = Vec::with_capacity(array.len());
    for (index, value) in array.iter().enumerate() {
        let item_label = format!("{label}[{index}]");
        let table = value.as_table().ok_or_else(|| {
            RepositoryConfigError::new(format!("{item_label} must contain exactly id and args"))
        })?;
        reject_exact(table, &["id", "args"], &item_label)?;
        let id = required_string(table, "id", &format!("{item_label}.id"))?;
        if !case_id_regex().is_match(&id) {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.id is invalid"
            )));
        }
        if !seen.insert(id.clone()) {
            return Err(RepositoryConfigError::new(format!(
                "{label} contains duplicate case id {id:?}"
            )));
        }
        let args = table.get("args").and_then(Value::as_array).ok_or_else(|| {
            RepositoryConfigError::new(format!(
                "{item_label}.args must be at most 64 non-empty bounded strings"
            ))
        })?;
        if args.len() > 64 {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.args must be at most 64 non-empty bounded strings"
            )));
        }
        let args = args
            .iter()
            .map(|argument| {
                argument
                    .as_str()
                    .filter(|argument| !argument.is_empty() && argument.len() <= 4096)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        RepositoryConfigError::new(format!(
                            "{item_label}.args must be at most 64 non-empty bounded strings"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if args.iter().map(String::len).sum::<usize>() > 65_536 {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.args exceeds 65536 UTF-8 bytes"
            )));
        }
        cases.push(CaseSpec {
            id,
            args,
            postgres: None,
        });
    }
    Ok(cases)
}

fn validate_diagnostic_sources(
    label: &str,
    value: Option<&Value>,
) -> Result<Vec<DiagnosticReportSource>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!("{label} must be an array of report tables"))
    })?;
    if array.len() > MAX_DIAGNOSTIC_SOURCES {
        return Err(RepositoryConfigError::new(format!(
            "{label} exceeds {MAX_DIAGNOSTIC_SOURCES} diagnostic sources"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();
    for (index, value) in array.iter().enumerate() {
        let item_label = format!("{label}[{index}]");
        let table = value.as_table().ok_or_else(|| {
            RepositoryConfigError::new(format!("{item_label} must contain exactly format and path"))
        })?;
        reject_exact(table, &["format", "path"], &item_label)?;
        let format =
            match required_string(table, "format", &format!("{item_label}.format"))?.as_str() {
                "junit" => DiagnosticReportFormat::Junit,
                "playwright-json" => DiagnosticReportFormat::PlaywrightJson,
                "rust-json" => DiagnosticReportFormat::RustJson,
                _ => {
                    return Err(RepositoryConfigError::new(format!(
                        "{item_label}.format must be 'junit', 'playwright-json', or 'rust-json'"
                    )));
                }
            };
        let path = required_string(table, "path", &format!("{item_label}.path"))?;
        if path.len() > 512 || validate_normalized_relative(&path).is_err() {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.path must be a normalized leaf diagnostics-relative path"
            )));
        }
        if !seen.insert((format, path.clone())) {
            return Err(RepositoryConfigError::new(format!(
                "{label} contains a duplicate diagnostic source"
            )));
        }
        sources.push(DiagnosticReportSource { format, path });
    }
    Ok(sources)
}

fn validate_retained_artifacts(
    label: &str,
    value: Option<&Value>,
) -> Result<Vec<RetainedArtifactSpec>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!(
            "{label} must be an array of retained-artifact tables"
        ))
    })?;
    if array.len() > MAX_RETAINED_ARTIFACTS {
        return Err(RepositoryConfigError::new(format!(
            "{label} exceeds {MAX_RETAINED_ARTIFACTS} entries"
        )));
    }
    let mut names = HashSet::new();
    let mut paths = Vec::<PathBuf>::new();
    let mut total = 0_u64;
    let mut artifacts = Vec::new();
    for (index, value) in array.iter().enumerate() {
        let item_label = format!("{label}[{index}]");
        let table = value.as_table().ok_or_else(|| {
            RepositoryConfigError::new(format!(
                "{item_label} must contain exactly name, path, and max_bytes"
            ))
        })?;
        reject_exact(table, &["name", "path", "max_bytes"], &item_label)?;
        let name = required_string(table, "name", &format!("{item_label}.name"))?;
        if !check_name_regex().is_match(&name) || !names.insert(name.clone()) {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.name is invalid or repeated"
            )));
        }
        let path = validate_artifact_path(
            &format!("{item_label}.path"),
            &required_string(table, "path", &format!("{item_label}.path"))?,
        )?;
        let parsed = PathBuf::from(&path);
        if parsed
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .is_some_and(|name| matches!(name, ".git" | ".devcoordinator"))
        {
            return Err(RepositoryConfigError::new(format!(
                "{item_label}.path is reserved"
            )));
        }
        if paths.iter().any(|prior| {
            parsed == *prior || parsed.starts_with(prior) || prior.starts_with(&parsed)
        }) {
            return Err(RepositoryConfigError::new(format!(
                "{label} contains overlapping paths"
            )));
        }
        let maximum = bounded_integer(
            table.get("max_bytes"),
            0,
            1,
            MAX_RETAINED_ARTIFACT_BYTES,
            &format!("{item_label}.max_bytes"),
        )?;
        total = total.saturating_add(maximum);
        if total > MAX_RETAINED_ARTIFACT_TOTAL_BYTES {
            return Err(RepositoryConfigError::new(format!(
                "{label} declared limits exceed {MAX_RETAINED_ARTIFACT_TOTAL_BYTES} bytes"
            )));
        }
        paths.push(parsed);
        artifacts.push(RetainedArtifactSpec {
            name,
            path,
            max_bytes: maximum,
        });
    }
    Ok(artifacts)
}

fn validate_artifact_path(label: &str, value: &str) -> Result<String, RepositoryConfigError> {
    if value.len() > 256
        || value.chars().any(char::is_control)
        || validate_normalized_relative(value).is_err()
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} must be a normalized repository-relative path"
        )));
    }
    Ok(value.to_owned())
}

fn validate_normalized_relative(value: &str) -> Result<(), ()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.to_string_lossy() != value
    {
        Err(())
    } else {
        Ok(())
    }
}

fn string_list(
    label: &str,
    value: Option<&Value>,
    nonempty: bool,
) -> Result<Vec<String>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        RepositoryConfigError::new(format!("{label} must be an array of strings"))
    })?;
    if nonempty && array.is_empty() {
        return Err(RepositoryConfigError::new(format!(
            "{label} must be a non-empty array of strings"
        )));
    }
    let values = array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                RepositoryConfigError::new(format!("{label} must be an array of strings"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.iter().collect::<HashSet<_>>().len() != values.len() {
        return Err(RepositoryConfigError::new(format!(
            "{label} contains duplicates"
        )));
    }
    Ok(values)
}

fn string_array(label: &str, value: Option<&Value>) -> Result<Vec<String>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a list of names")))?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                RepositoryConfigError::new(format!("{label} must be a list of names"))
            })
        })
        .collect()
}

fn validate_env(
    label: &str,
    value: Option<&Value>,
    reserve_internal: bool,
) -> Result<BTreeMap<String, String>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a table of strings")))?;
    let mut environment = BTreeMap::new();
    for (key, value) in table {
        if !environment_identifier(key) || value.as_str().is_none() {
            return Err(RepositoryConfigError::new(format!(
                "{label}.{key} must be a string with an identifier name"
            )));
        }
        if reserve_internal && key.starts_with("DEVCOORDINATOR_") {
            return Err(RepositoryConfigError::new(format!(
                "{label}.{key} uses a reserved internal prefix"
            )));
        }
        let value = value.as_str().expect("checked string");
        if secret_key_regex().is_match(key) && !value.is_empty() {
            return Err(RepositoryConfigError::new(format!(
                "{label}.{key} looks like a literal secret; reference secrets held outside the repository instead"
            )));
        }
        environment.insert(key.clone(), value.to_owned());
    }
    Ok(environment)
}

fn environment_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

fn validate_cwd(root: &Path, label: &str, value: &str) -> Result<PathBuf, RepositoryConfigError> {
    resolve_inside(root, value).map_err(|_| {
        RepositoryConfigError::new(format!(
            "{label} must be repository-relative and not escape the repository"
        ))
    })
}

fn resolve_inside(root: &Path, value: &str) -> Result<PathBuf, ()> {
    let relative = Path::new(value);
    if relative.is_absolute() {
        return Err(());
    }
    let mut lexical = root.to_owned();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => lexical.push(component),
            Component::ParentDir => {
                if lexical == root || !lexical.pop() || !lexical.starts_with(root) {
                    return Err(());
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(()),
        }
    }
    let resolved = canonicalize_allow_missing(&lexical)?;
    if resolved == root || resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(())
    }
}

fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf, ()> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let mut suffix = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(());
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(existing.file_name().ok_or(())?.to_os_string());
                existing = existing.parent().ok_or(())?;
            }
            Err(_) => return Err(()),
        }
    }
    let mut result = existing.canonicalize().map_err(|_| ())?;
    for component in suffix.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

fn bounded_integer(
    value: Option<&Value>,
    default: u64,
    minimum: u64,
    maximum: u64,
    label: &str,
) -> Result<u64, RepositoryConfigError> {
    let value = match value {
        Some(value) => value
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| {
                RepositoryConfigError::new(format!(
                    "{label} must be an integer in [{minimum}, {maximum}]"
                ))
            })?,
        None => default,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(RepositoryConfigError::new(format!(
            "{label} must be an integer in [{minimum}, {maximum}]"
        )));
    }
    Ok(value)
}

fn required_string(table: &Table, key: &str, label: &str) -> Result<String, RepositoryConfigError> {
    table
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a string")))
}

fn optional_string(
    table: &Table,
    key: &str,
    label: &str,
) -> Result<Option<String>, RepositoryConfigError> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| RepositoryConfigError::new(format!("{label} must be a string")))
        })
        .transpose()
}

fn parse_tier(value: String, label: &str) -> Result<ValidationTier, RepositoryConfigError> {
    match value.as_str() {
        "development" => Ok(ValidationTier::Development),
        "pre-merge" => Ok(ValidationTier::PreMerge),
        "release" => Ok(ValidationTier::Release),
        _ => Err(RepositoryConfigError::new(format!(
            "{label}.tier must be 'development', 'pre-merge', or 'release'"
        ))),
    }
}

const fn tier_rank(tier: ValidationTier) -> u8 {
    match tier {
        ValidationTier::Development => 0,
        ValidationTier::PreMerge => 1,
        ValidationTier::Release => 2,
    }
}

fn reject_unknown(
    table: &Table,
    allowed: &[&str],
    label: &str,
) -> Result<(), RepositoryConfigError> {
    let unknown = table
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(RepositoryConfigError::new(format!(
            "{label} unknown keys: {unknown:?}"
        )))
    }
}

fn reject_exact(
    table: &Table,
    expected: &[&str],
    label: &str,
) -> Result<(), RepositoryConfigError> {
    let actual = table.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        Err(RepositoryConfigError::new(format!(
            "{label} must contain exactly {}",
            expected.into_iter().collect::<Vec<_>>().join(", ")
        )))
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("String writes cannot fail");
    }
    result
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("constant regex"))
}

fn test_name_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[a-z0-9][a-z0-9-]{0,31}$")
}

fn check_name_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[a-z0-9][a-z0-9-]{0,63}$")
}

fn case_id_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
}

fn postgres_image_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^postgres:[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
}

fn postgres_digest_image_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(
        &VALUE,
        r"^[a-z0-9][a-z0-9._/-]{0,200}(?::[A-Za-z0-9][A-Za-z0-9._-]{0,127})?@sha256:[0-9a-f]{64}$",
    )
}

fn postgres_ident_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[a-z_][a-z0-9_]{0,62}$")
}

fn secret_key_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(
        &VALUE,
        r"(?i)(token|secret|password|passwd|credential|api_?key)",
    )
}

fn name_regex() -> &'static Regex {
    test_name_regex()
}

fn validate_deployment(
    root: &Path,
    name: &str,
    body: &Table,
) -> Result<DeploymentSpec, RepositoryConfigError> {
    let label = format!("[deployment.{name}]");
    reject_unknown(
        body,
        &[
            "source",
            "domain",
            "components",
            "build",
            "ttl_seconds",
            "component",
            "public",
        ],
        &label,
    )?;
    let sources = validate_sources(&label, body.get("source"))?;
    let domains = validate_domains(&label, body.get("domain"), &sources)?;
    let build = match body.get("build") {
        Some(value) => {
            let values = value.as_array().ok_or_else(|| {
                RepositoryConfigError::new(format!("{label} build must be an argv array"))
            })?;
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            RepositoryConfigError::new(format!(
                                "{label} build must be an argv array"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        None => Vec::new(),
    };
    let public = match body.get("public") {
        Some(value) => value.as_bool().ok_or_else(|| {
            RepositoryConfigError::new(format!("{label} public must be a boolean"))
        })?,
        None => false,
    };
    let ttl_seconds = body
        .get("ttl_seconds")
        .map(|_| {
            bounded_integer(
                body.get("ttl_seconds"),
                0,
                60,
                30 * 86_400,
                &format!("{label} ttl_seconds"),
            )
        })
        .transpose()?;
    let order = string_list(&format!("{label} components"), body.get("components"), true)?;
    if order.iter().any(|name| !name_regex().is_match(name)) {
        return Err(RepositoryConfigError::new(format!(
            "{label} components must be a non-empty list of names"
        )));
    }
    let tables = body
        .get("component")
        .and_then(Value::as_table)
        .ok_or_else(|| {
            RepositoryConfigError::new(format!("{label} component must be a table of tables"))
        })?;
    let missing = order
        .iter()
        .filter(|name| !tables.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    let extra = tables
        .keys()
        .filter(|name| !order.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() || !extra.is_empty() {
        return Err(RepositoryConfigError::new(format!(
            "{label} components and component tables differ: missing {missing:?}, unlisted {extra:?}"
        )));
    }
    let mut components = Vec::with_capacity(order.len());
    for (index, component_name) in order.iter().enumerate() {
        let table = tables[component_name].as_table().ok_or_else(|| {
            RepositoryConfigError::new(format!(
                "[deployment.{name}.component.{component_name}] must be a table"
            ))
        })?;
        components.push(validate_component(
            root,
            name,
            component_name,
            index,
            table,
            &order[..index],
        )?);
    }
    if sources.iter().any(|source| source == "checkout")
        && components.iter().any(|component| {
            component.kind == ComponentKind::Compose && component.compose_env_file.is_some()
        })
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} ignored Compose env_file requires worktree-only source"
        )));
    }
    let routes = components
        .iter()
        .filter(|component| component.route)
        .collect::<Vec<_>>();
    if routes.len() > 1 {
        return Err(RepositoryConfigError::new(format!(
            "{label} at most one component may set route = true"
        )));
    }
    if !domains.is_empty() && routes.is_empty() {
        let implicit = components
            .iter()
            .filter(|component| {
                component.wants_port
                    && matches!(
                        component.kind,
                        ComponentKind::Process | ComponentKind::Docker | ComponentKind::Compose
                    )
            })
            .count();
        if implicit != 1 {
            return Err(RepositoryConfigError::new(format!(
                "{label} domain requires one component with route = true (implicit only when exactly one process/docker/compose component leases a port)"
            )));
        }
    }
    if routes
        .first()
        .is_some_and(|component| !component.wants_port)
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} the route component must lease a port"
        )));
    }
    Ok(DeploymentSpec {
        name: name.to_owned(),
        sources,
        domains,
        build,
        ttl_seconds,
        public,
        components,
    })
}

fn validate_sources(
    label: &str,
    value: Option<&Value>,
) -> Result<Vec<String>, RepositoryConfigError> {
    let values = match value {
        None => vec!["worktree".to_owned()],
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    RepositoryConfigError::new(format!(
                        "{label} source must be 'worktree', 'checkout', or a list enabling both"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(RepositoryConfigError::new(format!(
                "{label} source must be 'worktree', 'checkout', or a list enabling both"
            )));
        }
    };
    if values.is_empty()
        || values
            .iter()
            .any(|source| !matches!(source.as_str(), "worktree" | "checkout"))
        || values.iter().collect::<HashSet<_>>().len() != values.len()
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} source must be 'worktree', 'checkout', or a list enabling both"
        )));
    }
    Ok(values)
}

fn validate_domains(
    label: &str,
    value: Option<&Value>,
    sources: &[String],
) -> Result<BTreeMap<String, String>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let values = match value {
        Value::String(value) if sources.len() == 1 => {
            BTreeMap::from([(sources[0].clone(), value.clone())])
        }
        Value::String(_) => {
            return Err(RepositoryConfigError::new(format!(
                "{label} with two sources, domain must be a table {{ checkout = ..., worktree = ... }}"
            )));
        }
        Value::Table(table) => table
            .iter()
            .map(|(source, value)| {
                value
                    .as_str()
                    .map(|value| (source.clone(), value.to_owned()))
                    .ok_or_else(|| {
                        RepositoryConfigError::new(format!(
                            "{label} domain must be a label or a per-source table"
                        ))
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        _ => {
            return Err(RepositoryConfigError::new(format!(
                "{label} domain must be a label or a per-source table"
            )));
        }
    };
    for (source, domain) in &values {
        if !sources.contains(source) {
            return Err(RepositoryConfigError::new(format!(
                "{label} domain.{source} names a source that is not enabled"
            )));
        }
        if !domain_label_regex().is_match(domain) {
            return Err(RepositoryConfigError::new(format!(
                "{label} domain.{source} must be a DNS label"
            )));
        }
    }
    if values.values().collect::<HashSet<_>>().len() != values.len() {
        return Err(RepositoryConfigError::new(format!(
            "{label} the two sources must not share a domain"
        )));
    }
    Ok(values)
}

fn validate_component(
    root: &Path,
    deployment_name: &str,
    name: &str,
    order: usize,
    body: &Table,
    earlier: &[String],
) -> Result<ComponentSpec, RepositoryConfigError> {
    let label = format!("[deployment.{deployment_name}.component.{name}]");
    let kind = match body.get("type").and_then(Value::as_str) {
        Some("process") => ComponentKind::Process,
        Some("docker") => ComponentKind::Docker,
        Some("compose") => ComponentKind::Compose,
        Some("postgres") => ComponentKind::Postgres,
        Some("external") => ComponentKind::External,
        _ => {
            return Err(RepositoryConfigError::new(format!(
                "{label} type must be one of process, docker, compose, postgres, external"
            )));
        }
    };
    let common = ["type", "depends_on", "env", "independent_control"];
    let specific: &[&str] = match kind {
        ComponentKind::Process => &[
            "command",
            "cwd",
            "port",
            "route",
            "health",
            "persistent_paths",
        ],
        ComponentKind::Docker => &["image", "command", "port", "volumes", "health"],
        ComponentKind::Compose => &[
            "file",
            "files",
            "env_file",
            "services",
            "finite_services",
            "independent_services",
            "build",
            "port",
            "route",
            "timeout_seconds",
        ],
        ComponentKind::Postgres => &["image", "database", "user", "shared_from"],
        ComponentKind::External => &["tcp"],
    };
    let mut allowed = common.to_vec();
    allowed.extend_from_slice(specific);
    reject_unknown(body, &allowed, &label)?;
    let depends_on = string_array(&format!("{label} depends_on"), body.get("depends_on"))?;
    if let Some(dependency) = depends_on
        .iter()
        .find(|dependency| !earlier.contains(*dependency))
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} depends_on {dependency:?} must name an earlier component"
        )));
    }
    let env = validate_env(&format!("{label} env"), body.get("env"), false)?;
    let independent_control = match body.get("independent_control") {
        Some(value) => value.as_bool().ok_or_else(|| {
            RepositoryConfigError::new(format!("{label} independent_control must be a boolean"))
        })?,
        None => true,
    };
    let mut specification = ComponentSpec {
        name: name.to_owned(),
        kind,
        order,
        independent_control,
        depends_on,
        env,
        command: Vec::new(),
        cwd: ".".into(),
        wants_port: false,
        route: false,
        health: None,
        persistent_paths: Vec::new(),
        image: None,
        container_port: None,
        volumes: Vec::new(),
        compose_files: Vec::new(),
        compose_env_file: None,
        compose_build: false,
        services: Vec::new(),
        finite_services: Vec::new(),
        independent_services: Vec::new(),
        compose_timeout_seconds: COMPOSE_READINESS_TIMEOUT_DEFAULT,
        database: None,
        user: None,
        shared_from: None,
        tcp: None,
    };
    match kind {
        ComponentKind::Process => {
            validate_process_component(root, body, &label, &mut specification)?
        }
        ComponentKind::Docker => validate_docker_component(body, &label, &mut specification)?,
        ComponentKind::Compose => {
            validate_compose_component(root, body, &label, &mut specification)?
        }
        ComponentKind::Postgres => validate_postgres_component(body, &label, &mut specification)?,
        ComponentKind::External => validate_external_component(body, &label, &mut specification)?,
    }
    if specification.is_finite_workload()
        && (specification.wants_port || specification.route || specification.health.is_some())
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} finite-only workloads cannot expose a port, route or service health probe"
        )));
    }
    Ok(specification)
}

fn validate_process_component(
    root: &Path,
    body: &Table,
    label: &str,
    specification: &mut ComponentSpec,
) -> Result<(), RepositoryConfigError> {
    specification.command = validate_command(
        &format!("{label} command"),
        body.get("command").ok_or_else(|| {
            RepositoryConfigError::new(format!("{label} command must be a non-empty argv array"))
        })?,
    )?;
    let cwd = optional_string(body, "cwd", &format!("{label} cwd"))?.unwrap_or_else(|| ".".into());
    resolve_inside(root, &cwd)
        .map_err(|_| RepositoryConfigError::new(format!("{label} cwd escapes the repository")))?;
    specification.cwd = cwd;
    specification.wants_port = match body.get("port") {
        Some(value) => value.as_bool().ok_or_else(|| {
            RepositoryConfigError::new(format!(
                "{label} port must be true/false (the daemon leases it)"
            ))
        })?,
        None => false,
    };
    specification.route = optional_bool(body, "route", false, label)?;
    if specification.route && !specification.wants_port {
        return Err(RepositoryConfigError::new(format!(
            "{label} route = true requires port = true"
        )));
    }
    specification.persistent_paths = match body.get("persistent_paths") {
        Some(value) => {
            let paths = string_array(&format!("{label} persistent_paths"), Some(value))?;
            if paths.iter().any(|path| {
                path.is_empty()
                    || Path::new(path).is_absolute()
                    || Path::new(path)
                        .components()
                        .any(|component| component == Component::ParentDir)
            }) {
                return Err(RepositoryConfigError::new(format!(
                    "{label} persistent_paths must be repository-relative"
                )));
            }
            paths
        }
        None => Vec::new(),
    };
    specification.health = validate_health(label, body.get("health"))?;
    Ok(())
}

fn validate_docker_component(
    body: &Table,
    label: &str,
    specification: &mut ComponentSpec,
) -> Result<(), RepositoryConfigError> {
    let image = required_string(body, "image", &format!("{label} image"))?;
    if !image_regex().is_match(&image) {
        return Err(RepositoryConfigError::new(format!(
            "{label} image must be a plain image reference"
        )));
    }
    specification.image = Some(image);
    specification.command = match body.get("command") {
        Some(value) => {
            let array = value.as_array().ok_or_else(|| {
                RepositoryConfigError::new(format!("{label} command must be an argv array"))
            })?;
            array
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        RepositoryConfigError::new(format!("{label} command must be an argv array"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        None => Vec::new(),
    };
    specification.container_port = body
        .get("port")
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    RepositoryConfigError::new(format!(
                        "{label} port must be a container port number"
                    ))
                })
        })
        .transpose()?;
    specification.wants_port = specification.container_port.is_some();
    specification.volumes = match body.get("volumes") {
        Some(value) => {
            let values = string_list(&format!("{label} volumes"), Some(value), false)?;
            if values.iter().any(|value| !volume_regex().is_match(value)) {
                return Err(RepositoryConfigError::new(format!(
                    "{label} volumes must be 'name:/container/path' named volumes (host paths are forbidden)"
                )));
            }
            values
        }
        None => Vec::new(),
    };
    specification.health = validate_health(label, body.get("health"))?;
    Ok(())
}

fn validate_compose_component(
    root: &Path,
    body: &Table,
    label: &str,
    specification: &mut ComponentSpec,
) -> Result<(), RepositoryConfigError> {
    if body.contains_key("file") && body.contains_key("files") {
        return Err(RepositoryConfigError::new(format!(
            "{label} file and files are mutually exclusive"
        )));
    }
    let files = if let Some(value) = body.get("files") {
        string_list(&format!("{label} files"), Some(value), true)?
    } else {
        vec![
            optional_string(body, "file", &format!("{label} file"))?
                .unwrap_or_else(|| "docker-compose.yml".into()),
        ]
    };
    if files.iter().any(|file| {
        file.is_empty() || Path::new(file).is_absolute() || resolve_inside(root, file).is_err()
    }) {
        return Err(RepositoryConfigError::new(format!(
            "{label} file escapes the repository or is not repository-relative"
        )));
    }
    specification.compose_files = files;
    specification.compose_env_file =
        optional_string(body, "env_file", &format!("{label} env_file"))?;
    if specification.compose_env_file.as_ref().is_some_and(|file| {
        file.is_empty() || Path::new(file).is_absolute() || resolve_inside(root, file).is_err()
    }) {
        return Err(RepositoryConfigError::new(format!(
            "{label} env_file escapes the repository or is not repository-relative"
        )));
    }
    specification.services = validate_compose_services(label, "services", body.get("services"))?;
    specification.finite_services =
        validate_compose_services(label, "finite_services", body.get("finite_services"))?;
    if !specification.finite_services.is_empty()
        && (specification.services.is_empty()
            || specification
                .finite_services
                .iter()
                .any(|service| !specification.services.contains(service)))
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} finite_services must be included in explicit services"
        )));
    }
    specification.independent_services = validate_compose_services(
        label,
        "independent_services",
        body.get("independent_services"),
    )?;
    if !specification.independent_services.is_empty()
        && (specification.services.is_empty()
            || specification
                .independent_services
                .iter()
                .any(|service| !specification.services.contains(service)))
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} independent_services must be included in explicit services"
        )));
    }
    let overlap = specification
        .independent_services
        .iter()
        .filter(|service| specification.finite_services.contains(service))
        .cloned()
        .collect::<Vec<_>>();
    if !overlap.is_empty() {
        return Err(RepositoryConfigError::new(format!(
            "{label} finite services cannot be independently controlled: {overlap:?}"
        )));
    }
    specification.compose_build = optional_bool(body, "build", false, label)?;
    specification.wants_port = optional_bool(body, "port", false, label)?;
    specification.route = optional_bool(body, "route", false, label)?;
    if specification.route && !specification.wants_port {
        return Err(RepositoryConfigError::new(format!(
            "{label} route = true requires port = true"
        )));
    }
    specification.compose_timeout_seconds = bounded_integer(
        body.get("timeout_seconds"),
        COMPOSE_READINESS_TIMEOUT_DEFAULT,
        1,
        if specification.finite_services.is_empty() {
            900
        } else {
            TIMEOUT_MAX
        },
        &format!("{label} timeout_seconds"),
    )?;
    Ok(())
}

fn validate_postgres_component(
    body: &Table,
    label: &str,
    specification: &mut ComponentSpec,
) -> Result<(), RepositoryConfigError> {
    if let Some(shared) = optional_string(body, "shared_from", &format!("{label} shared_from"))? {
        if !shared_from_regex().is_match(&shared) {
            return Err(RepositoryConfigError::new(format!(
                "{label} shared_from must be '<deployment_id>/<component>'"
            )));
        }
        if ["image", "database", "user"]
            .iter()
            .any(|key| body.contains_key(*key))
        {
            return Err(RepositoryConfigError::new(format!(
                "{label} shared_from excludes image/database/user"
            )));
        }
        specification.shared_from = Some(shared);
        return Ok(());
    }
    let image = optional_string(body, "image", &format!("{label} image"))?
        .unwrap_or_else(|| POSTGRES_IMAGE_DEFAULT.into());
    if !postgres_image_regex().is_match(&image) {
        return Err(RepositoryConfigError::new(format!(
            "{label} image must be an official 'postgres:<tag>'"
        )));
    }
    let database = optional_string(body, "database", &format!("{label} database"))?
        .unwrap_or_else(|| "app".into());
    let user =
        optional_string(body, "user", &format!("{label} user"))?.unwrap_or_else(|| "app".into());
    for (field, value) in [("database", &database), ("user", &user)] {
        if !postgres_ident_regex().is_match(value) {
            return Err(RepositoryConfigError::new(format!(
                "{label} {field} must match [a-z_][a-z0-9_]{{0,62}}"
            )));
        }
    }
    specification.image = Some(image);
    specification.database = Some(database);
    specification.user = Some(user);
    specification.wants_port = true;
    Ok(())
}

fn validate_external_component(
    body: &Table,
    label: &str,
    specification: &mut ComponentSpec,
) -> Result<(), RepositoryConfigError> {
    let tcp = required_string(body, "tcp", &format!("{label} tcp"))?;
    if !tcp_regex().is_match(&tcp) {
        return Err(RepositoryConfigError::new(format!(
            "{label} tcp must be 'host:port'"
        )));
    }
    specification.tcp = Some(tcp);
    Ok(())
}

fn validate_compose_services(
    label: &str,
    field: &str,
    value: Option<&Value>,
) -> Result<Vec<String>, RepositoryConfigError> {
    let values = string_list(&format!("{label} {field}"), value, false)?;
    if values
        .iter()
        .any(|service| !compose_service_regex().is_match(service))
    {
        return Err(RepositoryConfigError::new(format!(
            "{label} {field} must be a list of names"
        )));
    }
    Ok(values)
}

fn validate_health(
    label: &str,
    value: Option<&Value>,
) -> Result<Option<HealthSpec>, RepositoryConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| RepositoryConfigError::new(format!("{label} health must be a table")))?;
    reject_unknown(
        table,
        &["path", "tcp", "timeout_seconds"],
        &format!("{label} health"),
    )?;
    let timeout_seconds = bounded_integer(
        table.get("timeout_seconds"),
        HEALTH_TIMEOUT_DEFAULT,
        1,
        900,
        &format!("{label} health.timeout_seconds"),
    )?;
    if let Some(path) = table.get("path") {
        let path = path
            .as_str()
            .filter(|path| path.starts_with('/'))
            .ok_or_else(|| {
                RepositoryConfigError::new(format!("{label} health.path must start with '/'"))
            })?;
        return Ok(Some(HealthSpec {
            kind: HealthKind::Http,
            path: Some(path.to_owned()),
            timeout_seconds,
        }));
    }
    if table.get("tcp").and_then(Value::as_bool) == Some(true) {
        return Ok(Some(HealthSpec {
            kind: HealthKind::Tcp,
            path: None,
            timeout_seconds,
        }));
    }
    Err(RepositoryConfigError::new(format!(
        "{label} health needs path = '/...' or tcp = true"
    )))
}

fn optional_bool(
    table: &Table,
    key: &str,
    default: bool,
    label: &str,
) -> Result<bool, RepositoryConfigError> {
    match table.get(key) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RepositoryConfigError::new(format!("{label} {key} must be a boolean"))),
        None => Ok(default),
    }
}

fn compose_service_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
}

fn domain_label_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
}

fn image_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(
        &VALUE,
        r"^[a-z0-9][a-z0-9._/-]{0,200}(?::[A-Za-z0-9][A-Za-z0-9._-]{0,127})?(?:@sha256:[0-9a-f]{64})?$",
    )
}

fn volume_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^[a-z0-9][a-z0-9_-]{0,63}:(/[^:\s]*)$")
}

fn tcp_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^(?:127\.0\.0\.1|localhost|[a-z0-9.-]+):\d{1,5}$")
}

fn shared_from_regex() -> &'static Regex {
    static VALUE: OnceLock<Regex> = OnceLock::new();
    regex(&VALUE, r"^d[0-9a-f]{16}/[a-z0-9][a-z0-9-]{0,31}$")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn write(root: &Path, text: &str) {
        std::fs::write(root.join(CONFIG_NAME), text).expect("configuration");
    }

    #[test]
    fn full_test_graph_and_deployment_preserve_typed_defaults_and_canonical_identity() {
        let temporary = tempdir().expect("tempdir");
        std::fs::create_dir(temporary.path().join("sub")).expect("subdir");
        write(
            temporary.path(),
            r#"
schema = 2
[test]
default = "unit"
[test.unit]
timeout_seconds = 30
env = { CI = "1" }
[[test.unit.check]]
name = "source"
tier = "development"
role = "preflight"
command = ["source-check"]
invalidates = ["browser"]
[[test.unit.check]]
name = "server"
tier = "development"
command = ["run-server"]
completion = "event"
[[test.unit.check]]
name = "browser"
tier = "release"
command = ["node", "verify.mjs"]
requires = ["server"]
diagnostic_sources = [
  { format = "junit", path = "pytest/results.xml" },
  { format = "playwright-json", path = "browser/report.json" },
  { format = "rust-json", path = "rust/events.jsonl" },
]
retained_artifacts = [
  { name = "production", path = "artifacts/production", max_bytes = 536870912 },
  { name = "developer-test", path = "artifacts/developer-test", max_bytes = 134217728 },
]
[test.slow]
cwd = "sub"
[[test.slow.check]]
name = "main"
tier = "development"
cases = [{ id = "one", args = ["--id", "1"] }]
case_command = ["node", "case.mjs"]

[deployment.web]
source = ["checkout", "worktree"]
domain = { checkout = "app", worktree = "app-dev" }
components = ["db", "api", "worker", "cache", "stack", "smtp"]
build = ["npm", "run", "build"]
[deployment.web.component.db]
type = "postgres"
database = "app"
user = "app"
[deployment.web.component.api]
type = "process"
command = ["npm", "run", "start"]
port = true
route = true
health = { path = "/healthz", timeout_seconds = 30 }
depends_on = ["db"]
persistent_paths = ["var/data"]
[deployment.web.component.worker]
type = "process"
command = ["npm", "run", "worker"]
depends_on = ["db"]
independent_control = false
[deployment.web.component.cache]
type = "docker"
image = "valkey/valkey:9.1.0-alpine"
port = 6379
volumes = ["data:/data"]
[deployment.web.component.stack]
type = "compose"
files = ["compose.yml", "compose.build.yml"]
services = ["db", "bootstrap", "api"]
finite_services = ["bootstrap"]
independent_services = ["api"]
build = true
port = true
timeout_seconds = 900
[deployment.web.component.smtp]
type = "external"
tcp = "127.0.0.1:25"
"#,
        );
        let unit = load_test_spec(temporary.path(), None).expect("default test");
        assert_eq!(unit.name, "unit");
        assert_eq!(unit.timeout_seconds, 30);
        assert_eq!(unit.env.get("CI").map(String::as_str), Some("1"));
        assert_eq!(unit.config_digest.len(), 64);
        assert_eq!(unit.checks[1].completion, CompletionMode::Event);
        assert_eq!(unit.checks[2].requires, vec!["server", "source"]);
        assert_eq!(unit.checks[2].diagnostic_sources.len(), 3);
        assert_eq!(unit.checks[2].retained_artifacts.len(), 2);
        let summaries = validate_test_config(temporary.path()).expect("summaries");
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().any(|summary| summary.default));
        let slow = load_test_spec(temporary.path(), Some("slow")).expect("slow");
        assert_eq!(
            slow.cwd,
            temporary.path().join("sub").canonicalize().unwrap()
        );
        assert_eq!(slow.checks[0].cases.as_ref().map(Vec::len), Some(1));

        assert_eq!(
            list_deployment_names(temporary.path()).expect("names"),
            vec!["web"]
        );
        let deployment = load_deployment_spec(temporary.path(), "web").expect("deployment");
        assert_eq!(deployment.sources, vec!["checkout", "worktree"]);
        assert_eq!(deployment.domain_for("worktree"), Some("app-dev"));
        assert_eq!(
            deployment
                .route_component()
                .map(|value| value.name.as_str()),
            Some("api")
        );
        assert!(deployment.component("db").unwrap().owns_persistent_data());
        assert!(
            deployment
                .component("cache")
                .unwrap()
                .owns_persistent_data()
        );
        assert!(
            !deployment
                .component("worker")
                .unwrap()
                .owns_persistent_data()
        );
        assert_eq!(
            deployment.component("stack").unwrap().finite_services,
            vec!["bootstrap"]
        );
        assert_ne!(
            deployment.canonical("checkout"),
            deployment.canonical("worktree")
        );
        assert_ne!(
            deployment.fingerprint("checkout"),
            deployment.fingerprint("worktree")
        );
    }

    #[test]
    fn test_configuration_rejection_matrix_preserves_existing_classes() {
        let cases = [
            ("", "schema"),
            ("schema=1\n[test.u]\ncommand=['x']", "schema 1"),
            ("schema=2", "test.<name>"),
            (
                "schema=2\n[test.u]\ncommand=['x']\ntier='release'",
                "rejects direct",
            ),
            (
                "schema=2\n[test.u]\n[[test.u.check]]\nname='main'\ntier='release'\ncommand='sh -c x'",
                "never a shell",
            ),
            (
                "schema=2\n[test.u]\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=[]",
                "1..256",
            ),
            (
                "schema=2\n[test.u]\ncwd='../out'\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']",
                "escape",
            ),
            (
                "schema=2\n[test.u]\ntimeout_seconds=0\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']",
                "timeout",
            ),
            (
                "schema=2\nqueue=true\n[test.u]\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']",
                "top-level",
            ),
            (
                "schema=2\n[test.u]\nenv={API_TOKEN='abc'}\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']",
                "secret",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='a'\ntier='release'\ncommand=['x']\nafter=['missing']",
                "unknown dependencies",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='a'\ntier='release'\ncommand=['x']\nafter=['b']\n[[test.g.check]]\nname='b'\ntier='release'\ncommand=['x']\nafter=['a']",
                "cycle",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='a'\ntier='release'\ncommand=['x']\nproduces=['../secret']",
                "normalized",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='a'\ntier='release'\ncommand=['x']\nenv={DEVCOORDINATOR_RUN_ID='x'}",
                "reserved",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='case'\ntier='release'\ndiscover=['find']",
                "case_command",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='case'\ntier='release'\ncases=[{id='bad/id',args=[]}]\ncase_command=['run']",
                "id is invalid",
            ),
            (
                "schema=2\n[test.g]\n[[test.g.check]]\nname='gate'\ntier='release'\nrole='preflight'\ncommand=['x']\ninvalidates=[]",
                "invalidate",
            ),
        ];
        for (text, fragment) in cases {
            let temporary = tempdir().expect("tempdir");
            write(temporary.path(), text);
            let error = load_test_spec(temporary.path(), None).expect_err(text);
            assert!(
                error.message.to_lowercase().contains(fragment),
                "{fragment:?} not in {:?}",
                error.message
            );
        }
    }

    #[test]
    fn retained_diagnostic_postgres_and_symlink_rejections_are_exact() {
        for (declaration, fragment) in [
            (
                "retained_artifacts=[{name='same',path='artifacts',max_bytes=1},{name='nested',path='artifacts/nested',max_bytes=1}]",
                "overlapping",
            ),
            (
                "retained_artifacts=[{name='private',path='.devcoordinator/private',max_bytes=1}]",
                "reserved",
            ),
            (
                "diagnostic_sources=[{format='tap',path='report.tap'}]",
                "format",
            ),
            (
                "diagnostic_sources=[{format='junit',path='../report.xml'}]",
                "diagnostics-relative",
            ),
        ] {
            let temporary = tempdir().expect("tempdir");
            write(
                temporary.path(),
                &format!(
                    "schema=2\n[test.u]\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']\n{declaration}"
                ),
            );
            let error = load_test_spec(temporary.path(), None).expect_err(declaration);
            assert!(error.message.contains(fragment), "{}", error.message);
        }
        for image in [
            format!("postgres@sha256:{}", "1".repeat(64)),
            format!("postgis/postgis@sha256:{}", "a".repeat(64)),
        ] {
            let temporary = tempdir().expect("tempdir");
            write(
                temporary.path(),
                &format!(
                    "schema=2\n[test.u]\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']\n[test.u.postgres]\nimage='{image}'"
                ),
            );
            assert_eq!(
                load_test_spec(temporary.path(), None)
                    .unwrap()
                    .postgres
                    .unwrap()
                    .image,
                image
            );
        }
        let parent = tempdir().expect("parent");
        let outside = parent.path().join("outside");
        let repository = parent.path().join("repo");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(&repository).unwrap();
        symlink(&outside, repository.join("link")).unwrap();
        write(
            &repository,
            "schema=2\n[test.u]\ncwd='link'\n[[test.u.check]]\nname='main'\ntier='release'\ncommand=['x']",
        );
        assert!(
            load_test_spec(&repository, None)
                .unwrap_err()
                .message
                .contains("escape")
        );
    }

    #[test]
    fn explicitly_finite_workload_needs_no_running_service() {
        let temporary = tempdir().expect("tempdir");
        write(
            temporary.path(),
            "schema=2\n[deployment.check]\ncomponents=['probe']\n[deployment.check.component.probe]\ntype='compose'\nservices=['probe']\nfinite_services=['probe']\n",
        );
        let spec = load_deployment_spec(temporary.path(), "check").expect("finite workload");
        assert!(spec.components[0].is_finite_workload());
        assert!(!spec.components[0].wants_port);
        assert!(spec.route_component().is_none());
    }

    #[test]
    fn finite_container_execution_deadlines_are_explicit_and_bounded() {
        let temporary = tempdir().expect("tempdir");
        for services in ["['probe']", "['probe','api']"] {
            let base = format!(
                "schema=2\n[deployment.check]\ncomponents=['probe']\n[deployment.check.component.probe]\ntype='compose'\nservices={services}\nfinite_services=['probe']\n"
            );
            for timeout in [901, 3300, TIMEOUT_MAX] {
                write(
                    temporary.path(),
                    &format!("{base}timeout_seconds={timeout}\n"),
                );
                let spec = load_deployment_spec(temporary.path(), "check")
                    .expect("bounded finite execution");
                assert_eq!(spec.components[0].compose_timeout_seconds, timeout);
            }
            write(temporary.path(), &base);
            let spec = load_deployment_spec(temporary.path(), "check").expect("default deadline");
            assert_eq!(
                spec.components[0].compose_timeout_seconds,
                COMPOSE_READINESS_TIMEOUT_DEFAULT
            );
            for timeout in ["0", "-1", "21601", "3300.5", "'3300'", "true"] {
                write(
                    temporary.path(),
                    &format!("{base}timeout_seconds={timeout}\n"),
                );
                let error = load_deployment_spec(temporary.path(), "check")
                    .expect_err("invalid finite deadline");
                assert!(
                    error.message.contains("timeout_seconds"),
                    "{}",
                    error.message
                );
            }
        }
        for finite in ["", "finite_services=[]\n"] {
            let base = format!(
                "schema=2\n[deployment.check]\ncomponents=['api']\n[deployment.check.component.api]\ntype='compose'\nservices=['api']\n{finite}"
            );
            write(temporary.path(), &format!("{base}timeout_seconds=900\n"));
            assert!(load_deployment_spec(temporary.path(), "check").is_ok());
            write(temporary.path(), &format!("{base}timeout_seconds=901\n"));
            let error = load_deployment_spec(temporary.path(), "check")
                .expect_err("ordinary readiness remains bounded");
            assert!(error.message.contains("[1, 900]"), "{}", error.message);
        }
    }

    #[test]
    fn deployment_rejection_matrix_and_implicit_route_match_existing_contract() {
        let base = "schema=2\n[deployment.d]\ncomponents=['a']\n";
        let cases = [
            (
                "[deployment.d.component.a]\ntype='process'\ncommand='sh -c x'",
                "argv array",
            ),
            (
                "[deployment.d.component.a]\ntype='process'\ncommand=['x']\nport=8080",
                "daemon leases",
            ),
            (
                "[deployment.d.component.a]\ntype='process'\ncommand=['x']\nroute=true",
                "requires port",
            ),
            (
                "domain='x'\n[deployment.d.component.a]\ntype='process'\ncommand=['x']",
                "requires one component",
            ),
            (
                "[deployment.d.component.a]\ntype='docker'\nimage='img'\nvolumes=['/host:/data']",
                "host paths",
            ),
            (
                "[deployment.d.component.a]\ntype='postgres'\nshared_from='bad'",
                "shared_from",
            ),
            (
                "[deployment.d.component.a]\ntype='compose'\nfile='../x.yml'",
                "escapes",
            ),
            (
                "[deployment.d.component.a]\ntype='external'\ntcp='nope'",
                "host:port",
            ),
            (
                "ttl_seconds=5\n[deployment.d.component.a]\ntype='process'\ncommand=['x']",
                "ttl_seconds",
            ),
            (
                "[deployment.d.component.a]\ntype='compose'\nservices=['bootstrap']\nfinite_services=['bootstrap']\nport=true",
                "finite-only",
            ),
            (
                "[deployment.d.component.a]\ntype='compose'\nroute=true",
                "requires port",
            ),
            (
                "[deployment.d.component.a]\ntype='compose'\ntimeout_seconds=901",
                "[1, 900]",
            ),
        ];
        for (body, fragment) in cases {
            let temporary = tempdir().expect("tempdir");
            write(temporary.path(), &format!("{base}{body}\n"));
            let error = load_deployment_spec(temporary.path(), "d").expect_err(body);
            assert!(error.message.contains(fragment), "{}", error.message);
        }

        let temporary = tempdir().expect("tempdir");
        write(
            temporary.path(),
            "schema=2\n[deployment.d]\ncomponents=['app','worker']\ndomain='para'\n[deployment.d.component.app]\ntype='process'\ncommand=['x']\nport=true\n[deployment.d.component.worker]\ntype='process'\ncommand=['y']",
        );
        let deployment = load_deployment_spec(temporary.path(), "d").expect("implicit route");
        assert_eq!(
            deployment
                .route_component()
                .map(|component| component.name.as_str()),
            Some("app")
        );
    }

    #[test]
    fn configuration_file_itself_is_bounded_regular_and_nofollow() {
        let temporary = tempdir().expect("tempdir");
        let outside = temporary.path().join("outside.toml");
        std::fs::write(&outside, "schema=2").unwrap();
        symlink(&outside, temporary.path().join(CONFIG_NAME)).unwrap();
        assert!(load_test_spec(temporary.path(), None).is_err());
        std::fs::remove_file(temporary.path().join(CONFIG_NAME)).unwrap();
        std::fs::write(
            temporary.path().join(CONFIG_NAME),
            vec![b'x'; MAX_CONFIG_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            load_test_spec(temporary.path(), None)
                .unwrap_err()
                .message
                .contains("exceeds")
        );
    }
}
