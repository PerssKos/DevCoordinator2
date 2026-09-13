//! Instance configuration with thin-client/private-daemon separation.

use std::collections::{HashMap, HashSet};
use std::ffi::c_char;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use rustix::fs::{Mode, OFlags, fstat, open};
use serde::Deserialize;
use thiserror::Error;

const INSTALLED_ENV_PATH: &str = "/etc/devcoordinator2/instance.env";
const MAX_POLICY_BYTES: u64 = 65_536;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexUsageSource {
    pub uid: u32,
    pub codex_home: PathBuf,
    pub executable: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub socket_path: PathBuf,
    pub sandbox_bridge_dir: PathBuf,
    pub state_dir: PathBuf,
    pub unit_prefix: String,
    pub slice_name: String,
    pub client_group: String,
    pub port_range: (u16, u16),
    pub base_domain: String,
    pub edge_uid: Option<u32>,
    pub admin_emails: Vec<String>,
    pub telegram_token_file: Option<PathBuf>,
    pub telegram_api: String,
    pub bugs_dir: PathBuf,
    pub compose_env_allowlist_file: Option<PathBuf>,
    pub compose_env_authorizations: HashSet<(String, String)>,
    pub codex_usage_sources_file: Option<PathBuf>,
    pub codex_usage_sources: Vec<CodexUsageSource>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrivatePolicy {
    pub compose_authorizations: bool,
    pub codex_usage_sources: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_with(PrivatePolicy::default())
    }

    pub fn load_for_daemon() -> Result<Self, ConfigError> {
        Self::load_with(PrivatePolicy {
            compose_authorizations: true,
            codex_usage_sources: true,
        })
    }

    pub fn load_with(policy: PrivatePolicy) -> Result<Self, ConfigError> {
        let file_values = instance_file_values();
        let value = |key: &str, default: &str| {
            std::env::var(key)
                .ok()
                .filter(|candidate| !candidate.is_empty())
                .or_else(|| {
                    file_values
                        .get(key)
                        .filter(|candidate| !candidate.is_empty())
                        .cloned()
                })
                .unwrap_or_else(|| default.to_owned())
        };
        let socket_path: PathBuf =
            value("DEVCOORDINATOR2_SOCKET", "/run/devcoordinator2/daemon.sock").into();
        let port_range = parse_port_range(&value("DEVCOORDINATOR2_PORT_RANGE", "20000-29999"))?;
        let compose_path = optional_path(&value("DEVCOORDINATOR2_COMPOSE_ENV_ALLOWLIST_FILE", ""));
        let usage_path = optional_path(&value("DEVCOORDINATOR2_CODEX_USAGE_SOURCES_FILE", ""));
        let edge_uid_value = value("DEVCOORDINATOR2_EDGE_UID", "");
        let edge_uid = if edge_uid_value.is_empty() {
            None
        } else {
            Some(edge_uid_value.parse().map_err(|_| {
                ConfigError::Invalid("DEVCOORDINATOR2_EDGE_UID must be an unsigned integer".into())
            })?)
        };
        let mut base_domain = value("DEVCOORDINATOR2_BASE_DOMAIN", "").trim().to_owned();
        while base_domain.starts_with('.') {
            base_domain.remove(0);
        }
        while base_domain.ends_with('.') {
            base_domain.pop();
        }
        let sandbox_bridge_dir = sandbox_bridge_directory(&socket_path);
        Ok(Self {
            socket_path,
            sandbox_bridge_dir,
            state_dir: value("DEVCOORDINATOR2_STATE_DIR", "/var/lib/devcoordinator2").into(),
            unit_prefix: value("DEVCOORDINATOR2_UNIT_PREFIX", "devcoordinator2-test"),
            slice_name: value("DEVCOORDINATOR2_SLICE", "devcoordinator2-tests.slice"),
            client_group: value("DEVCOORDINATOR2_CLIENT_GROUP", "devcoordinator2-clients"),
            port_range,
            base_domain,
            edge_uid,
            admin_emails: value("DEVCOORDINATOR2_ADMIN_EMAILS", "")
                .split(',')
                .map(str::trim)
                .filter(|email| !email.is_empty())
                .map(str::to_lowercase)
                .collect(),
            telegram_token_file: optional_path(&value("DEVCOORDINATOR2_TELEGRAM_TOKEN_FILE", "")),
            telegram_api: value("DEVCOORDINATOR2_TELEGRAM_API", "https://api.telegram.org")
                .trim_end_matches('/')
                .to_owned(),
            bugs_dir: value("DEVCOORDINATOR2_BUGS_DIR", "/var/lib/devcoordinator2-bugs").into(),
            compose_env_allowlist_file: compose_path.clone(),
            compose_env_authorizations: if policy.compose_authorizations {
                read_compose_policy(compose_path.as_deref())?
            } else {
                HashSet::new()
            },
            codex_usage_sources_file: usage_path.clone(),
            codex_usage_sources: if policy.codex_usage_sources {
                read_usage_policy(usage_path.as_deref())?
            } else {
                Vec::new()
            },
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.state_dir.join("authority.sqlite3")
    }

    pub fn capacity_socket_path(&self) -> PathBuf {
        self.socket_path.with_file_name("capacity.sock")
    }

    pub fn deployments_dir(&self) -> PathBuf {
        self.state_dir.join("deployments")
    }

    pub fn secrets_dir(&self) -> PathBuf {
        self.state_dir.join("secrets")
    }

    pub fn routes_path(&self) -> PathBuf {
        self.state_dir.join("public/routes.json")
    }

    pub fn deploy_unit_prefix(&self) -> String {
        format!("{}-deploy", self.unit_prefix.replace("-test", ""))
    }
}

pub fn sandbox_bridge_directory(socket_path: &Path) -> PathBuf {
    let file_values = instance_file_values();
    std::env::var_os("DEVCOORDINATOR2_SANDBOX_BRIDGE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            file_values
                .get("DEVCOORDINATOR2_SANDBOX_BRIDGE_DIR")
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| crate::sandbox_bridge::default_directory(socket_path))
}

fn optional_path(value: &str) -> Option<PathBuf> {
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn parse_port_range(value: &str) -> Result<(u16, u16), ConfigError> {
    let (low, high) = value.split_once('-').ok_or_else(|| {
        ConfigError::Invalid("DEVCOORDINATOR2_PORT_RANGE must be 'low-high'".into())
    })?;
    let low: u16 = low.parse().map_err(|_| {
        ConfigError::Invalid("DEVCOORDINATOR2_PORT_RANGE must be 'low-high'".into())
    })?;
    let high: u16 = high.parse().map_err(|_| {
        ConfigError::Invalid("DEVCOORDINATOR2_PORT_RANGE must be 'low-high'".into())
    })?;
    if low < 1024 || low >= high {
        return Err(ConfigError::Invalid(
            "DEVCOORDINATOR2_PORT_RANGE must lie within 1024-65535".into(),
        ));
    }
    Ok((low, high))
}

fn instance_file_values() -> HashMap<String, String> {
    let candidates = match std::env::var_os("DEVCOORDINATOR2_INSTANCE_ENV") {
        Some(explicit) => vec![PathBuf::from(explicit)],
        None => vec![
            std::env::current_dir().unwrap_or_default().join(".env"),
            PathBuf::from(INSTALLED_ENV_PATH),
        ],
    };
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| parse_env(&text))
        .unwrap_or_default()
}

fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, raw_value) = line.split_once('=')?;
            let key = key.trim();
            if !key.starts_with("DEVCOORDINATOR2_") {
                return None;
            }
            let value = raw_value.trim();
            let value = if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposePolicy {
    schema: u8,
    authorizations: Vec<ComposeAuthorization>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeAuthorization {
    repository_id: String,
    path: String,
}

pub(crate) fn read_compose_policy(
    path: Option<&Path>,
) -> Result<HashSet<(String, String)>, ConfigError> {
    let Some(path) = path else {
        return Ok(HashSet::new());
    };
    let policy: ComposePolicy = read_private_json(path, "Compose environment allowlist")?;
    if policy.schema != 1 || policy.authorizations.len() > 256 {
        return Err(ConfigError::Invalid(
            "Compose environment allowlist must be schema 1 with at most 256 authorizations".into(),
        ));
    }
    let repository = Regex::new(r"^r[0-9a-f]{16}$").expect("static regex");
    let mut result = HashSet::new();
    for authorization in policy.authorizations {
        if !repository.is_match(&authorization.repository_id) {
            return Err(ConfigError::Invalid(
                "Compose environment authorization repository_id is invalid".into(),
            ));
        }
        if !valid_relative_path(&authorization.path) {
            return Err(ConfigError::Invalid(
                "Compose environment authorization path must be normalized relative".into(),
            ));
        }
        if !result.insert((authorization.repository_id, authorization.path)) {
            return Err(ConfigError::Invalid(
                "Compose environment allowlist contains a duplicate entry".into(),
            ));
        }
    }
    Ok(result)
}

pub(crate) fn valid_relative_path(value: &str) -> bool {
    if value.is_empty() || value.len() > 512 || value.contains(['\\', '\0']) {
        return false;
    }
    let path = Path::new(value);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && path.components().collect::<PathBuf>().to_string_lossy() == value
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePolicy {
    schema: u8,
    sources: Vec<UsagePolicySource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePolicySource {
    uid: u32,
    codex_home: String,
    executable: String,
}

fn read_usage_policy(path: Option<&Path>) -> Result<Vec<CodexUsageSource>, ConfigError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let policy: UsagePolicy = read_private_json(path, "Codex usage source policy")?;
    if policy.schema != 1 || policy.sources.len() > 32 {
        return Err(ConfigError::Invalid(
            "Codex usage source policy must be schema 1 with at most 32 sources".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut sources = Vec::with_capacity(policy.sources.len());
    for source in policy.sources {
        if source.uid == 0 || !seen.insert(source.uid) || !uid_exists(source.uid) {
            return Err(ConfigError::Invalid(
                "Codex usage source uid is invalid, missing, or duplicated".into(),
            ));
        }
        let codex_home = validate_absolute_policy_path("codex_home", source.codex_home)?;
        let executable = validate_absolute_policy_path("executable", source.executable)?;
        sources.push(CodexUsageSource {
            uid: source.uid,
            codex_home,
            executable,
        });
    }
    Ok(sources)
}

fn validate_absolute_policy_path(name: &str, value: String) -> Result<PathBuf, ConfigError> {
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || !Path::new(&value).is_absolute()
    {
        return Err(ConfigError::Invalid(format!(
            "Codex usage source {name} must be a valid absolute path"
        )));
    }
    Ok(value.into())
}

fn read_private_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    label: &str,
) -> Result<T, ConfigError> {
    if !path.is_absolute() {
        return Err(ConfigError::Invalid(format!(
            "{label} path must be absolute"
        )));
    }
    let descriptor = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    let status = fstat(&descriptor).map_err(|error| ConfigError::Io {
        path: path.to_owned(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    if status.st_mode & libc::S_IFMT != libc::S_IFREG
        || status.st_size < 0
        || status.st_size as u64 > MAX_POLICY_BYTES
        || status.st_mode & 0o022 != 0
        || (status.st_uid != 0 && status.st_uid != rustix::process::geteuid().as_raw())
    {
        return Err(ConfigError::Invalid(format!(
            "{label} must be a small regular non-symlink file with a trusted owner and private writes"
        )));
    }
    let file = std::fs::File::from(descriptor);
    let mut bytes = Vec::with_capacity(status.st_size as usize);
    file.take(MAX_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_POLICY_BYTES {
        return Err(ConfigError::Invalid(format!("{label} exceeds 65536 bytes")));
    }
    serde_json::from_slice(&bytes).map_err(|source| ConfigError::Json {
        path: path.to_owned(),
        source,
    })
}

fn uid_exists(uid: u32) -> bool {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: all pointers reference valid writable storage for the duration of
    // the reentrant libc call; success is accepted only with a non-null result.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len(),
            &mut result,
        )
    };
    status == 0 && !result.is_null()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_environment_file_syntax() {
        let values = parse_env(
            "# ignored\nDEVCOORDINATOR2_SOCKET='/tmp/x.sock'\nOTHER=no\nDEVCOORDINATOR2_STATE_DIR = /tmp/state\n",
        );
        assert_eq!(values["DEVCOORDINATOR2_SOCKET"], "/tmp/x.sock");
        assert_eq!(values["DEVCOORDINATOR2_STATE_DIR"], "/tmp/state");
        assert!(!values.contains_key("OTHER"));
    }

    #[test]
    fn validates_port_range_and_relative_policy_paths() {
        assert_eq!(parse_port_range("20000-29999").unwrap(), (20000, 29999));
        assert!(parse_port_range("80-90").is_err());
        assert!(valid_relative_path("deploy/dev.env"));
        assert!(!valid_relative_path("../dev.env"));
        assert!(!valid_relative_path("/dev.env"));
        assert!(!valid_relative_path("deploy\\dev.env"));
    }
}
