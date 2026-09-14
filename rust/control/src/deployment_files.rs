//! No-follow deployment runtime files and root-private PostgreSQL credentials.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{self as unix_fs, AtFlags, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::platform::{HostRandom, RandomSource};

const MAX_SECRET_BYTES: u64 = 4 * 1024;
const MAX_LOG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentFormat {
    Systemd,
    Docker,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresCredentials {
    pub user: String,
    pub database: String,
    pub password: String,
}

#[derive(Debug, Error)]
pub enum DeploymentFileError {
    #[error("invalid deployment filesystem request: {0}")]
    Invalid(String),
    #[error("deployment filesystem operation failed: {0}")]
    Io(String),
    #[error("secure random source is unavailable")]
    Random,
}

#[derive(Clone)]
pub struct DeploymentFiles {
    deployments_root: PathBuf,
    secrets_root: PathBuf,
    random: Arc<dyn RandomSource>,
}

impl DeploymentFiles {
    pub fn new(deployments_root: PathBuf, secrets_root: PathBuf) -> Self {
        Self::with_random(deployments_root, secrets_root, Arc::new(HostRandom))
    }

    pub fn with_random(
        deployments_root: PathBuf,
        secrets_root: PathBuf,
        random: Arc<dyn RandomSource>,
    ) -> Self {
        Self {
            deployments_root,
            secrets_root,
            random,
        }
    }

    pub fn deployment_dir(&self, deployment_id: &str) -> Result<PathBuf, DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        Ok(self.deployments_root.join(deployment_id))
    }

    pub fn generation_path(
        &self,
        deployment_id: &str,
        generation: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        if generation == 0 {
            return Err(DeploymentFileError::Invalid(
                "generation must be positive".into(),
            ));
        }
        Ok(self
            .deployment_dir(deployment_id)?
            .join(format!("gen-{generation}")))
    }

    pub fn environment_path(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        validate_component(component)?;
        Ok(self
            .deployment_dir(deployment_id)?
            .join("env")
            .join(format!("{component}-g{generation}.env")))
    }

    pub fn postgres_environment_path(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        validate_component(component)?;
        Ok(self
            .deployment_dir(deployment_id)?
            .join("env")
            .join(format!("{component}-g{generation}.pg.env")))
    }

    pub fn log_path(
        &self,
        deployment_id: &str,
        component: &str,
    ) -> Result<PathBuf, DeploymentFileError> {
        validate_component_or_build(component)?;
        Ok(self
            .deployment_dir(deployment_id)?
            .join("logs")
            .join(format!("{component}.log")))
    }

    pub fn scratch_path(
        &self,
        deployment_id: &str,
        generation: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        Ok(self
            .deployment_dir(deployment_id)?
            .join("tmp")
            .join(format!("g{generation}")))
    }

    pub fn ensure_layout(
        &self,
        deployment_id: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let root = open_directory_path(&self.deployments_root, true, 0o755)?
            .ok_or_else(|| DeploymentFileError::Io("deployment root is unavailable".into()))?;
        let deployment = open_child_directory(&root, deployment_id, true, 0o755)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        for (name, mode, owner) in [
            ("env", 0o755, None),
            ("logs", 0o755, Some((caller_uid, caller_gid))),
            ("tmp", 0o700, Some((caller_uid, caller_gid))),
        ] {
            let directory =
                open_child_directory(&deployment, name, true, mode)?.ok_or_else(|| {
                    DeploymentFileError::Io("runtime directory is unavailable".into())
                })?;
            unix_fs::fchmod(&directory, Mode::from_raw_mode(mode as _))
                .map_err(|error| io_error("set runtime directory mode", error))?;
            if let Some((uid, gid)) = owner {
                fchown(&directory, uid, gid)?;
            }
        }
        Ok(())
    }

    pub fn ensure_scratch(
        &self,
        deployment_id: &str,
        generation: u32,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        self.ensure_layout(deployment_id, caller_uid, caller_gid)?;
        let deployment = self
            .open_deployment(deployment_id, false)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        let tmp = open_child_directory(&deployment, "tmp", false, 0o700)?
            .ok_or_else(|| DeploymentFileError::Io("scratch root is unavailable".into()))?;
        let name = format!("g{generation}");
        let directory = open_child_directory(&tmp, &name, true, 0o700)?
            .ok_or_else(|| DeploymentFileError::Io("scratch directory is unavailable".into()))?;
        unix_fs::fchmod(&directory, Mode::from_raw_mode(0o700))
            .map_err(|error| io_error("set scratch directory mode", error))?;
        fchown(&directory, caller_uid, caller_gid)?;
        self.scratch_path(deployment_id, generation)
    }

    pub fn allow_checkout_generation_creation(
        &self,
        deployment_id: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentFileError> {
        self.ensure_layout(deployment_id, caller_uid, caller_gid)?;
        let deployment = self
            .open_deployment(deployment_id, false)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        fchown(&deployment, caller_uid, caller_gid)
    }

    pub fn open_log_writer(
        &self,
        deployment_id: &str,
        component: &str,
        owner_uid: u32,
        owner_gid: u32,
    ) -> Result<File, DeploymentFileError> {
        self.open_log_file(deployment_id, component, owner_uid, owner_gid, false)
    }

    pub fn open_log_append(
        &self,
        deployment_id: &str,
        component: &str,
        owner_uid: u32,
        owner_gid: u32,
    ) -> Result<File, DeploymentFileError> {
        self.open_log_file(deployment_id, component, owner_uid, owner_gid, true)
    }

    fn open_log_file(
        &self,
        deployment_id: &str,
        component: &str,
        owner_uid: u32,
        owner_gid: u32,
        append: bool,
    ) -> Result<File, DeploymentFileError> {
        self.ensure_layout(deployment_id, owner_uid, owner_gid)?;
        validate_component_or_build(component)?;
        let deployment = self
            .open_deployment(deployment_id, false)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        let logs = open_child_directory(&deployment, "logs", false, 0o755)?
            .ok_or_else(|| DeploymentFileError::Io("log directory is unavailable".into()))?;
        let name = format!("{component}.log");
        let descriptor = unix_fs::openat(
            &logs,
            name.as_str(),
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | if append {
                    OFlags::APPEND
                } else {
                    OFlags::TRUNC
                },
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| io_error("open deployment log writer", error))?;
        let file = File::from(descriptor);
        unix_fs::fchmod(&file, Mode::from_raw_mode(0o600))
            .map_err(|error| io_error("set deployment log mode", error))?;
        fchown(&file, owner_uid, owner_gid)?;
        Ok(file)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write_environment(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
        values: &BTreeMap<String, String>,
        format: EnvironmentFormat,
        owner_uid: u32,
        owner_gid: u32,
    ) -> Result<PathBuf, DeploymentFileError> {
        self.ensure_layout(deployment_id, owner_uid, owner_gid)?;
        let payload = encode_environment(values, format)?;
        let deployment = self
            .open_deployment(deployment_id, false)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        let environment =
            open_child_directory(&deployment, "env", false, 0o755)?.ok_or_else(|| {
                DeploymentFileError::Io("environment directory is unavailable".into())
            })?;
        let name = format!("{component}-g{generation}.env");
        atomic_write(
            &environment,
            &name,
            &payload,
            0o600,
            owner_uid,
            owner_gid,
            self.random.as_ref(),
        )?;
        self.environment_path(deployment_id, component, generation)
    }

    pub fn write_postgres_environment(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
        credentials: &PostgresCredentials,
    ) -> Result<PathBuf, DeploymentFileError> {
        self.ensure_layout(
            deployment_id,
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        )?;
        let values = BTreeMap::from([
            ("POSTGRES_USER".into(), credentials.user.clone()),
            ("POSTGRES_PASSWORD".into(), credentials.password.clone()),
            ("POSTGRES_DB".into(), credentials.database.clone()),
        ]);
        let payload = encode_environment(&values, EnvironmentFormat::Docker)?;
        let target = self.postgres_environment_path(deployment_id, component, generation)?;
        let deployment = self
            .open_deployment(deployment_id, false)?
            .ok_or_else(|| DeploymentFileError::Io("deployment directory is unavailable".into()))?;
        let environment =
            open_child_directory(&deployment, "env", false, 0o755)?.ok_or_else(|| {
                DeploymentFileError::Io("environment directory is unavailable".into())
            })?;
        let target_name = target
            .file_name()
            .ok_or_else(|| DeploymentFileError::Invalid("environment path has no name".into()))?;
        atomic_write(
            &environment,
            target_name.to_str().ok_or_else(|| {
                DeploymentFileError::Invalid("environment name is not UTF-8".into())
            })?,
            &payload,
            0o600,
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
            self.random.as_ref(),
        )?;
        Ok(target)
    }

    pub fn postgres_credentials(
        &self,
        deployment_id: &str,
        component: &str,
        user: &str,
        database: &str,
    ) -> Result<PostgresCredentials, DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        validate_component(component)?;
        validate_credential_atom(user, "PostgreSQL user")?;
        validate_credential_atom(database, "PostgreSQL database")?;
        let directory = self
            .open_secret_directory(deployment_id, true)?
            .ok_or_else(|| DeploymentFileError::Io("secret directory is unavailable".into()))?;
        let name = format!("{component}.json");
        if let Some(existing) = read_secret(&directory, &name)?
            && existing.user == user
            && existing.database == database
        {
            return Ok(existing);
        }
        let mut random = [0_u8; 24];
        self.random
            .fill(&mut random)
            .map_err(|_| DeploymentFileError::Random)?;
        let credentials = PostgresCredentials {
            user: user.into(),
            database: database.into(),
            password: base64_url_no_pad(&random), // public-artifact-guard: allow text-secret
        };
        let payload = serde_json::to_vec(&credentials)
            .map_err(|error| DeploymentFileError::Invalid(error.to_string()))?;
        atomic_write(
            &directory,
            &name,
            &payload,
            0o600,
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
            self.random.as_ref(),
        )?;
        Ok(credentials)
    }

    pub fn read_postgres_credentials(
        &self,
        deployment_id: &str,
        component: &str,
    ) -> Result<Option<PostgresCredentials>, DeploymentFileError> {
        validate_component(component)?;
        let Some(directory) = self.open_secret_directory(deployment_id, false)? else {
            return Ok(None);
        };
        read_secret(&directory, &format!("{component}.json"))
    }

    pub fn delete_secrets(&self, deployment_id: &str) -> Result<(), DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let Some(root) = open_directory_path(&self.secrets_root, false, 0o700)? else {
            return Ok(());
        };
        let Some(directory) = open_child_directory(&root, deployment_id, false, 0o700)? else {
            return Ok(());
        };
        remove_contents(&directory)?;
        unix_fs::unlinkat(&root, deployment_id, AtFlags::REMOVEDIR)
            .map_err(|error| io_error("remove deployment secret directory", error))?;
        root.sync_all()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))
    }

    pub fn remove_generation_tree(
        &self,
        deployment_id: &str,
        generation: u32,
    ) -> Result<(), DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let Some(deployment) = self.open_deployment(deployment_id, false)? else {
            return Ok(());
        };
        let name = format!("gen-{generation}");
        let Some(directory) = open_child_directory(&deployment, &name, false, 0o700)? else {
            return Ok(());
        };
        remove_contents(&directory)?;
        unix_fs::unlinkat(&deployment, name.as_str(), AtFlags::REMOVEDIR)
            .map_err(|error| io_error("remove generation directory", error))?;
        deployment
            .sync_all()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))
    }

    pub fn cleanup_runtime_files(&self, deployment_id: &str) -> Result<(), DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let Some(root) = open_directory_path(&self.deployments_root, false, 0o755)? else {
            return Ok(());
        };
        let Some(deployment) = open_child_directory(&root, deployment_id, false, 0o755)? else {
            return Ok(());
        };
        remove_contents(&deployment)?;
        unix_fs::unlinkat(&root, deployment_id, AtFlags::REMOVEDIR)
            .map_err(|error| io_error("remove deployment runtime directory", error))?;
        root.sync_all()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))
    }

    pub fn read_log_tail(
        &self,
        deployment_id: &str,
        component: &str,
        tail_lines: u16,
    ) -> Result<(String, bool), DeploymentFileError> {
        if tail_lines == 0 || tail_lines > 5_000 {
            return Err(DeploymentFileError::Invalid(
                "tail_lines must be in 1..5000".into(),
            ));
        }
        validate_component_or_build(component)?;
        let Some(deployment) = self.open_deployment(deployment_id, false)? else {
            return Ok((String::new(), false));
        };
        let Some(logs) = open_child_directory(&deployment, "logs", false, 0o755)? else {
            return Ok((String::new(), false));
        };
        let name = format!("{component}.log");
        let descriptor = match unix_fs::openat(
            &logs,
            name.as_str(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(rustix::io::Errno::NOENT) => return Ok((String::new(), false)),
            Err(error) => return Err(io_error("open deployment log", error)),
        };
        let mut file = File::from(descriptor);
        let metadata = file
            .metadata()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        if !metadata.is_file() {
            return Err(DeploymentFileError::Invalid(
                "deployment log is not a regular file".into(),
            ));
        }
        let start = metadata.len().saturating_sub(MAX_LOG_BYTES);
        file.seek(SeekFrom::Start(start))
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take(MAX_LOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        bytes.truncate(MAX_LOG_BYTES as usize);
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().collect::<Vec<_>>();
        let first = lines.len().saturating_sub(usize::from(tail_lines));
        Ok((lines[first..].join("\n"), start > 0 || first > 0))
    }

    pub fn validate_repository_file(
        &self,
        worktree: &Path,
        relative: &str,
    ) -> Result<PathBuf, DeploymentFileError> {
        if !worktree.is_absolute() {
            return Err(DeploymentFileError::Invalid(
                "repository root must be absolute".into(),
            ));
        }
        let parts = safe_relative_parts(relative)?;
        let mut directory = unix_fs::open(
            worktree,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| io_error("open repository root", error))?;
        for part in &parts[..parts.len() - 1] {
            directory = unix_fs::openat(
                &directory,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| io_error("open repository file parent", error))?;
        }
        let file = unix_fs::openat(
            &directory,
            &parts[parts.len() - 1],
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| io_error("open repository file", error))?;
        if !file
            .metadata()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?
            .is_file()
        {
            return Err(DeploymentFileError::Invalid(
                "repository environment path must be a regular file".into(),
            ));
        }
        Ok(worktree.join(relative))
    }

    fn open_deployment(
        &self,
        deployment_id: &str,
        create: bool,
    ) -> Result<Option<File>, DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let Some(root) = open_directory_path(&self.deployments_root, create, 0o755)? else {
            return Ok(None);
        };
        open_child_directory(&root, deployment_id, create, 0o755)
    }

    fn open_secret_directory(
        &self,
        deployment_id: &str,
        create: bool,
    ) -> Result<Option<File>, DeploymentFileError> {
        validate_deployment_id(deployment_id)?;
        let Some(root) = open_directory_path(&self.secrets_root, create, 0o700)? else {
            return Ok(None);
        };
        unix_fs::fchmod(&root, Mode::from_raw_mode(0o700))
            .map_err(|error| io_error("set secret root mode", error))?;
        let directory = open_child_directory(&root, deployment_id, create, 0o700)?;
        if let Some(directory) = &directory {
            unix_fs::fchmod(directory, Mode::from_raw_mode(0o700))
                .map_err(|error| io_error("set secret directory mode", error))?;
        }
        Ok(directory)
    }
}

fn validate_deployment_id(value: &str) -> Result<(), DeploymentFileError> {
    if value.len() == 17
        && value.starts_with('d')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(DeploymentFileError::Invalid(
            "deployment ID must be d plus 16 hexadecimal characters".into(),
        ))
    }
}

fn validate_component(value: &str) -> Result<(), DeploymentFileError> {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(DeploymentFileError::Invalid(
            "component name is not a safe path atom".into(),
        ))
    }
}

fn validate_component_or_build(value: &str) -> Result<(), DeploymentFileError> {
    if value == "build" {
        Ok(())
    } else {
        validate_component(value)
    }
}

fn validate_credential_atom(value: &str, label: &str) -> Result<(), DeploymentFileError> {
    if !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Ok(())
    } else {
        Err(DeploymentFileError::Invalid(format!(
            "{label} is not a safe identifier"
        )))
    }
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn encode_environment(
    values: &BTreeMap<String, String>,
    format: EnvironmentFormat,
) -> Result<Vec<u8>, DeploymentFileError> {
    let mut payload = Vec::new();
    for (name, value) in values {
        if !valid_environment_name(name) {
            return Err(DeploymentFileError::Invalid(format!(
                "invalid environment name {name:?}"
            )));
        }
        if value.contains(['\n', '\r']) {
            return Err(DeploymentFileError::Invalid(format!(
                "environment value for {name} must be a single line"
            )));
        }
        let value = match format {
            EnvironmentFormat::Systemd => {
                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
            }
            EnvironmentFormat::Docker => value.clone(),
        };
        writeln!(payload, "{name}={value}")
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
    }
    Ok(payload)
}

fn safe_relative_parts(relative: &str) -> Result<Vec<OsString>, DeploymentFileError> {
    let path = Path::new(relative);
    if relative.is_empty() || path.is_absolute() {
        return Err(DeploymentFileError::Invalid(
            "repository file path must be relative".into(),
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if !name.is_empty() => parts.push(name.to_owned()),
            _ => {
                return Err(DeploymentFileError::Invalid(
                    "repository file path contains unsafe traversal".into(),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(DeploymentFileError::Invalid(
            "repository file path is empty".into(),
        ));
    }
    Ok(parts)
}

fn open_directory_path(
    path: &Path,
    create: bool,
    mode: u32,
) -> Result<Option<File>, DeploymentFileError> {
    if path.as_os_str().is_empty() {
        return Err(DeploymentFileError::Invalid(
            "runtime directory path is empty".into(),
        ));
    }
    let anchor = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = unix_fs::open(
        anchor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error("open runtime directory anchor", error))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(DeploymentFileError::Invalid(
                    "runtime directory path contains parent traversal".into(),
                ));
            }
        };
        if create {
            match unix_fs::mkdirat(&directory, name, Mode::from_raw_mode(mode as _)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(io_error("create runtime directory", error)),
            }
        }
        directory = match unix_fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(descriptor) => File::from(descriptor),
            Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
            Err(error) => return Err(io_error("open runtime directory", error)),
        };
    }
    Ok(Some(directory))
}

fn open_child_directory(
    parent: &File,
    name: &str,
    create: bool,
    mode: u32,
) -> Result<Option<File>, DeploymentFileError> {
    if name.is_empty() || name.as_bytes().contains(&b'/') {
        return Err(DeploymentFileError::Invalid(
            "runtime child name is not a path atom".into(),
        ));
    }
    if create {
        match unix_fs::mkdirat(parent, name, Mode::from_raw_mode(mode as _)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(io_error("create runtime child directory", error)),
        }
    }
    match unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(error) => Err(io_error("open runtime child directory", error)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn atomic_write(
    directory: &File,
    target: &str,
    payload: &[u8],
    mode: u32,
    owner_uid: u32,
    owner_gid: u32,
    random: &dyn RandomSource,
) -> Result<(), DeploymentFileError> {
    if target.is_empty() || target.as_bytes().contains(&b'/') {
        return Err(DeploymentFileError::Invalid(
            "atomic target is not a path atom".into(),
        ));
    }
    let mut suffix = [0_u8; 8];
    random
        .fill(&mut suffix)
        .map_err(|_| DeploymentFileError::Random)?;
    let temporary = format!(".{target}.{}.tmp", lower_hex(&suffix));
    let result = (|| {
        let descriptor = unix_fs::openat(
            directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(mode as _),
        )
        .map_err(|error| io_error("create atomic deployment file", error))?;
        let mut file = File::from(descriptor);
        file.write_all(payload)
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        unix_fs::fchmod(&file, Mode::from_raw_mode(mode as _))
            .map_err(|error| io_error("set deployment file mode", error))?;
        fchown(&file, owner_uid, owner_gid)?;
        file.sync_all()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        unix_fs::renameat(directory, temporary.as_str(), directory, target)
            .map_err(|error| io_error("publish atomic deployment file", error))?;
        directory
            .sync_all()
            .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unix_fs::unlinkat(directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn read_secret(
    directory: &File,
    name: &str,
) -> Result<Option<PostgresCredentials>, DeploymentFileError> {
    let descriptor = match unix_fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(io_error("open deployment secret", error)),
    };
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_SECRET_BYTES {
        return Err(DeploymentFileError::Invalid(
            "deployment secret is not a bounded regular file".into(),
        ));
    }
    let mut payload = Vec::new();
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|error| DeploymentFileError::Io(error.to_string()))?;
    if payload.len() as u64 > MAX_SECRET_BYTES {
        return Err(DeploymentFileError::Invalid(
            "deployment secret exceeds the size limit".into(),
        ));
    }
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|_| DeploymentFileError::Invalid("deployment secret is invalid".into()))
}

fn remove_contents(directory: &File) -> Result<(), DeploymentFileError> {
    let mut names = Vec::new();
    let mut entries =
        Dir::read_from(directory).map_err(|error| io_error("list deployment directory", error))?;
    for entry in &mut entries {
        let entry = entry.map_err(|error| io_error("read deployment directory", error))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(CString::new(bytes).map_err(|_| {
                DeploymentFileError::Invalid("directory entry contains NUL".into())
            })?);
        }
    }
    for name in names {
        match unix_fs::openat(
            directory,
            name.as_c_str(),
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let child = File::from(descriptor);
                remove_contents(&child)?;
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::REMOVEDIR)
                    .map_err(|error| io_error("remove deployment subdirectory", error))?;
            }
            Err(_) => {
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::empty())
                    .map_err(|error| io_error("remove deployment file", error))?;
            }
        }
    }
    directory
        .sync_all()
        .map_err(|error| DeploymentFileError::Io(error.to_string()))
}

fn fchown(file: &File, uid: u32, gid: u32) -> Result<(), DeploymentFileError> {
    // SAFETY: `file` owns a valid descriptor and fchown does not retain it.
    let result = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(DeploymentFileError::Io(
            io::Error::last_os_error().to_string(),
        ))
    }
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from((second & 0x0f) << 2 | third >> 6)] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0x3f)] as char);
        }
    }
    output
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[usize::from(byte >> 4)] as char);
        value.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    value
}

fn io_error(operation: &str, error: rustix::io::Errno) -> DeploymentFileError {
    DeploymentFileError::Io(format!("{operation}: {}", io::Error::from(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    #[derive(Clone, Copy)]
    struct FixedRandom;

    impl RandomSource for FixedRandom {
        fn fill(&self, destination: &mut [u8]) -> Result<(), getrandom::Error> {
            for (index, byte) in destination.iter_mut().enumerate() {
                *byte = u8::try_from(index % 251).unwrap();
            }
            Ok(())
        }
    }

    fn files(root: &Path) -> DeploymentFiles {
        DeploymentFiles::with_random(
            root.join("deployments"),
            root.join("secrets"),
            Arc::new(FixedRandom),
        )
    }

    #[test]
    fn environment_and_credentials_are_atomic_private_and_stable() {
        let temporary = tempdir().unwrap();
        let files = files(temporary.path());
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let deployment = "d1111111111111111";
        let environment = files
            .write_environment(
                deployment,
                "api",
                1,
                &BTreeMap::from([
                    ("PLAIN".into(), "value".into()),
                    ("QUOTED".into(), "a\\\"b".into()),
                ]),
                EnvironmentFormat::Systemd,
                uid,
                gid,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(&environment).unwrap(),
            "PLAIN=\"value\"\nQUOTED=\"a\\\\\\\"b\"\n"
        );
        assert_eq!(
            fs::metadata(&environment).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let first = files
            .postgres_credentials(deployment, "db", "app", "app")
            .unwrap();
        let second = files
            .postgres_credentials(deployment, "db", "app", "app")
            .unwrap();
        assert_eq!(first.password, second.password);
        assert_eq!(first.password.len(), 32);
        let secret = temporary.path().join("secrets/d1111111111111111/db.json");
        assert_eq!(
            fs::metadata(secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn repository_file_validation_rejects_symlink_and_traversal() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repository");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("real.env"), "A=1\n").unwrap();
        symlink(root.join("real.env"), root.join("linked.env")).unwrap();
        let files = files(temporary.path());
        assert_eq!(
            files.validate_repository_file(&root, "real.env").unwrap(),
            root.join("real.env")
        );
        assert!(files.validate_repository_file(&root, "linked.env").is_err());
        assert!(
            files
                .validate_repository_file(&root, "../real.env")
                .is_err()
        );
    }

    #[test]
    fn cleanup_unlinks_symlinks_without_following_them() {
        let temporary = tempdir().unwrap();
        let files = files(temporary.path());
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let deployment = "d2222222222222222";
        files.ensure_layout(deployment, uid, gid).unwrap();
        let outside = temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("preserved"), "yes").unwrap();
        symlink(
            &outside,
            files.deployment_dir(deployment).unwrap().join("logs/link"),
        )
        .unwrap();
        files.cleanup_runtime_files(deployment).unwrap();
        assert_eq!(
            fs::read_to_string(outside.join("preserved")).unwrap(),
            "yes"
        );
        assert!(!files.deployment_dir(deployment).unwrap().exists());
    }

    #[test]
    fn bounded_log_tail_uses_lines_and_reports_earlier_bytes() {
        let temporary = tempdir().unwrap();
        let files = files(temporary.path());
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let deployment = "d3333333333333333";
        files.ensure_layout(deployment, uid, gid).unwrap();
        let log = files.log_path(deployment, "api").unwrap();
        fs::write(&log, "one\ntwo\nthree\n").unwrap();
        assert_eq!(
            files.read_log_tail(deployment, "api", 2).unwrap(),
            ("two\nthree".into(), true)
        );
    }

    #[test]
    fn urlsafe_encoding_matches_unpadded_shape() {
        assert_eq!(base64_url_no_pad(&[0, 1, 2]), "AAEC");
        assert_eq!(base64_url_no_pad(&[255]), "_w");
        assert_eq!(base64_url_no_pad(&[255, 238]), "_-4");
    }
}
