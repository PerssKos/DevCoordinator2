//! Commit-stamped release build and installation planning.
//!
//! The canonical checkout remains the only source and binary tree. This
//! module proves a clean `main`, builds all release executables as the
//! checkout owner, verifies their embedded commit, records hashes in one
//! private manifest, and renders direct unit/link targets. Host mutation is
//! kept behind explicit command and filesystem boundaries for cutover tests.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use rusqlite::{Connection, OpenFlags};
use rustix::fs::{Mode, OFlags, open as unix_open};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const OUTPUT_CAP: usize = 64 * 1024;
const TEMPLATE_CAP: u64 = 256 * 1024;
const MANIFEST_SCHEMA: u8 = 1;
const BINARY_NAMES: &[&str] = &[
    "devcoordinator2",
    "devcoordinator2-executor",
    "devcoordinator2-tooling",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
    pub clear_environment: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostRunner;

impl CommandRunner for HostRunner {
    fn run(&self, request: &CommandRequest) -> Result<CommandOutput, String> {
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if request.clear_environment {
            command.env_clear();
        }
        command.envs(&request.environment);
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", request.program.display()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "command stdout was not captured".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "command stderr was not captured".to_owned())?;
        let stdout = std::thread::spawn(move || read_bounded_stream(stdout));
        let stderr = std::thread::spawn(move || read_bounded_stream(stderr));
        let status = child
            .wait()
            .map_err(|error| format!("cannot wait for {}: {error}", request.program.display()))?;
        let (stdout, stdout_truncated) = stdout
            .join()
            .map_err(|_| "command stdout reader failed".to_owned())??;
        let (stderr, stderr_truncated) = stderr
            .join()
            .map_err(|_| "command stderr reader failed".to_owned())??;
        Ok(CommandOutput {
            success: status.success(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BuildTools {
    pub cargo: PathBuf,
    pub setpriv: PathBuf,
}

impl Default for BuildTools {
    fn default() -> Self {
        Self {
            cargo: PathBuf::from("/usr/bin/cargo"),
            setpriv: PathBuf::from("/usr/bin/setpriv"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildIdentity {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryReceipt {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub source_commit: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallManifest {
    pub schema: u8,
    pub product: String,
    pub package_version: String,
    pub source_root: String,
    pub source_commit: String,
    pub built_at: String,
    pub binaries: Vec<BinaryReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationPlan {
    pub source_root: String,
    pub source_commit: String,
    pub manifest_path: String,
    pub daemon_unit_path: String,
    pub edge_unit_path: String,
    pub binary_links: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfigReceipt {
    pub worktree: String,
    pub tests: Vec<String>,
    pub deployments: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeAuthorization {
    pub repository_id: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexUsageSource {
    pub uid: u32,
    pub codex_home: String,
    pub executable: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalAccount {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityReceipt {
    pub client_group: String,
    pub client_gid: u32,
    pub edge_uid: u32,
    pub edge_gid: u32,
    pub client_accounts: Vec<String>,
}

pub trait IdentityDirectory {
    fn group_gid(&self, name: &str) -> Result<Option<u32>, String>;
    fn account(&self, name: &str) -> Result<Option<LocalAccount>, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostIdentityDirectory;

impl IdentityDirectory for HostIdentityDirectory {
    fn group_gid(&self, name: &str) -> Result<Option<u32>, String> {
        group_by_name(name)
    }

    fn account(&self, name: &str) -> Result<Option<LocalAccount>, String> {
        match account_by_name(name) {
            Ok(account) => Ok(Some(account)),
            Err(error) if error.contains("does not exist") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparePaths {
    pub etc_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub bugs_dir: PathBuf,
    pub edge_state_dir: PathBuf,
    pub tmpfiles_target: PathBuf,
}

impl Default for PreparePaths {
    fn default() -> Self {
        Self {
            etc_dir: "/etc/devcoordinator2".into(),
            runtime_dir: "/run/devcoordinator2".into(),
            state_dir: "/var/lib/devcoordinator2".into(),
            bugs_dir: "/var/lib/devcoordinator2-bugs".into(),
            edge_state_dir: "/var/lib/devcoordinator2-edge".into(),
            tmpfiles_target: "/etc/tmpfiles.d/devcoordinator2.conf".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrepareRequest {
    pub source_root: PathBuf,
    pub base_domain: String,
    pub admin_emails: String,
    pub identities: IdentityReceipt,
    pub edge_account: LocalAccount,
    pub system_owner: (u32, u32),
    pub canary: bool,
    pub canary_port: u16,
    pub compose_authorizations: Vec<ComposeAuthorization>,
    pub usage_sources: Vec<CodexUsageSource>,
    pub paths: PreparePaths,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareReceipt {
    pub source_root: String,
    pub created_instance_env: bool,
    pub created_edge_env: bool,
    pub created_session_secret: bool,
    pub compose_policy_changed: bool,
    pub usage_policy_changed: bool,
    pub edge_source_acl_entries: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposePolicy {
    schema: u8,
    authorizations: Vec<ComposeAuthorization>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePolicy {
    schema: u8,
    sources: Vec<CodexUsageSource>,
}

pub fn validate_live_checkout<R: CommandRunner>(
    root: &Path,
    fetch: bool,
    runner: &R,
) -> Result<String, String> {
    let lexical = std::path::absolute(root)
        .map_err(|error| format!("cannot make live checkout absolute: {error}"))?;
    let canonical = lexical
        .canonicalize()
        .map_err(|error| format!("cannot resolve live checkout: {error}"))?;
    if lexical != canonical
        || lexical
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true)
    {
        return Err(format!(
            "live checkout must be a real absolute directory: {}",
            lexical.display()
        ));
    }
    let top = git(&canonical, ["rev-parse", "--show-toplevel"], runner)?;
    let top = PathBuf::from(top.trim())
        .canonicalize()
        .map_err(|error| format!("cannot resolve Git worktree root: {error}"))?;
    if top != canonical {
        return Err("live checkout must be the Git worktree root".to_owned());
    }
    if fetch {
        git(&canonical, ["fetch", "origin", "main"], runner)?;
    }
    let branch = git(&canonical, ["branch", "--show-current"], runner)?;
    if branch.trim() != "main" {
        return Err(format!(
            "live checkout must be on main, found {}",
            if branch.trim().is_empty() {
                "detached HEAD"
            } else {
                branch.trim()
            }
        ));
    }
    let status = git(
        &canonical,
        ["status", "--porcelain", "--untracked-files=all"],
        runner,
    )?;
    if !status.trim().is_empty() {
        return Err("live checkout must be clean".to_owned());
    }
    let head = git(&canonical, ["rev-parse", "HEAD"], runner)?;
    let upstream = git(
        &canonical,
        ["rev-parse", "refs/remotes/origin/main"],
        runner,
    )?;
    let head = head.trim();
    if head != upstream.trim() {
        return Err("live checkout must exactly match fetched origin/main".to_owned());
    }
    validate_commit(head)?;
    Ok(head.to_owned())
}

pub fn checkout_build_identity(source_root: &Path) -> Result<BuildIdentity, String> {
    let manifest = source_root.join("Cargo.toml");
    let metadata = manifest
        .symlink_metadata()
        .map_err(|_| "Rust Cargo manifest has no valid non-root owner".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.uid() == 0 {
        return Err("Rust Cargo manifest must be owned by a usable non-root writer".to_owned());
    }
    let (gid, home) = passwd_identity(metadata.uid())?;
    if !home.is_dir() {
        return Err("Rust Cargo manifest must be owned by a usable non-root writer".to_owned());
    }
    Ok(BuildIdentity {
        uid: metadata.uid(),
        gid,
        home,
    })
}

pub fn build_release_binaries<R: CommandRunner>(
    source_root: &Path,
    source_commit: &str,
    identity: &BuildIdentity,
    tools: &BuildTools,
    runner: &R,
) -> Result<Vec<BinaryReceipt>, String> {
    validate_commit(source_commit)?;
    validate_tool(&tools.cargo, "Cargo")?;
    validate_tool(&tools.setpriv, "setpriv")?;
    let manifest = source_root.join("Cargo.toml");
    validate_regular(&manifest, false)
        .map_err(|_| "Rust Cargo manifest is unavailable".to_owned())?;
    let request = CommandRequest {
        program: tools.setpriv.clone(),
        args: vec![
            format!("--reuid={}", identity.uid).into(),
            format!("--regid={}", identity.gid).into(),
            "--init-groups".into(),
            "--reset-env".into(),
            "--".into(),
            "/usr/bin/env".into(),
            "-i".into(),
            format!("HOME={}", identity.home.display()).into(),
            "PATH=/usr/bin:/bin".into(),
            format!("DEVCOORDINATOR2_SOURCE_COMMIT={source_commit}").into(),
            format!("CARGO_TARGET_DIR={}", source_root.join("target").display()).into(),
            tools.cargo.as_os_str().to_owned(),
            "build".into(),
            "--locked".into(),
            "--release".into(),
            "--manifest-path".into(),
            manifest.as_os_str().to_owned(),
            "--workspace".into(),
        ],
        environment: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        clear_environment: true,
    };
    let output = runner.run(&request)?;
    if !output.success {
        return Err(format!(
            "Rust release build failed: {}",
            useful_failure(&output)
        ));
    }
    verify_release_binaries(source_root, source_commit, runner)
}

pub fn verify_release_binaries<R: CommandRunner + ?Sized>(
    source_root: &Path,
    source_commit: &str,
    runner: &R,
) -> Result<Vec<BinaryReceipt>, String> {
    let mut receipts = Vec::new();
    for name in BINARY_NAMES {
        let path = source_root.join("target/release").join(name);
        let metadata = validate_regular(&path, true).map_err(|error| {
            format!("Rust release binary {name} is unavailable or unsafe: {error}")
        })?;
        let output = runner.run(&CommandRequest {
            program: path.clone(),
            args: vec!["--source-commit".into()],
            environment: BTreeMap::new(),
            clear_environment: true,
        })?;
        if !output.success || output.stdout_truncated || output.stdout.trim() != source_commit {
            return Err(format!(
                "Rust release binary {name} does not embed source commit {source_commit}"
            ));
        }
        receipts.push(BinaryReceipt {
            name: (*name).to_owned(),
            path: path_text(&path)?,
            sha256: hash_file(&path)?,
            bytes: metadata.len(),
            source_commit: source_commit.to_owned(),
        });
    }
    Ok(receipts)
}

pub fn manifest(
    source_root: &Path,
    source_commit: &str,
    built_at: &str,
    binaries: Vec<BinaryReceipt>,
) -> Result<InstallManifest, String> {
    if binaries.len() != BINARY_NAMES.len()
        || BINARY_NAMES
            .iter()
            .any(|name| !binaries.iter().any(|binary| binary.name == *name))
    {
        return Err("installation manifest requires all release binaries".to_owned());
    }
    Ok(InstallManifest {
        schema: MANIFEST_SCHEMA,
        product: "devcoordinator2".to_owned(),
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
        source_root: path_text(source_root)?,
        source_commit: source_commit.to_owned(),
        built_at: built_at.to_owned(),
        binaries,
    })
}

pub fn write_manifest(
    path: &Path,
    manifest: &InstallManifest,
    owner: (u32, u32),
) -> Result<(), String> {
    let mut payload = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("cannot encode installation manifest: {error}"))?;
    payload.push(b'\n');
    atomic_file(path, &payload, 0o600, Some(owner))
}

pub fn read_and_verify_manifest<R: CommandRunner + ?Sized>(
    path: &Path,
    runner: &R,
) -> Result<InstallManifest, String> {
    read_and_verify_manifest_owned(path, runner, 0)
}

pub fn read_and_verify_manifest_owned<R: CommandRunner + ?Sized>(
    path: &Path,
    runner: &R,
    expected_uid: u32,
) -> Result<InstallManifest, String> {
    let file = unix_open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open installation manifest: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect installation manifest: {error}"))?;
    if !metadata.is_file()
        || metadata.len() > 256 * 1024
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != expected_uid
    {
        return Err(
            "installation manifest must be a root-owned private bounded regular file".to_owned(),
        );
    }
    let mut raw = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(|error| format!("cannot read installation manifest: {error}"))?;
    let manifest: InstallManifest = serde_json::from_slice(&raw)
        .map_err(|error| format!("installation manifest is invalid: {error}"))?;
    if manifest.schema != MANIFEST_SCHEMA
        || manifest.product != "devcoordinator2"
        || manifest.package_version != env!("CARGO_PKG_VERSION")
    {
        return Err("installation manifest identity is unsupported".to_owned());
    }
    let verified = verify_release_binaries(
        Path::new(&manifest.source_root),
        &manifest.source_commit,
        runner,
    )?;
    if verified != manifest.binaries {
        return Err("installed release binaries no longer match their manifest".to_owned());
    }
    Ok(manifest)
}

pub fn installation_plan(
    manifest: &InstallManifest,
    manifest_path: &Path,
) -> Result<InstallationPlan, String> {
    let source_root = Path::new(&manifest.source_root);
    let links = BTreeMap::from([
        (
            "/usr/local/bin/devcoordinator2".to_owned(),
            path_text(&source_root.join("target/release/devcoordinator2"))?,
        ),
        (
            "/usr/local/bin/devcoordinator2-tooling".to_owned(),
            path_text(&source_root.join("target/release/devcoordinator2-tooling"))?,
        ),
    ]);
    Ok(InstallationPlan {
        source_root: manifest.source_root.clone(),
        source_commit: manifest.source_commit.clone(),
        manifest_path: path_text(manifest_path)?,
        daemon_unit_path: "/etc/systemd/system/devcoordinator2.service".to_owned(),
        edge_unit_path: "/etc/systemd/system/devcoordinator2-edge.service".to_owned(),
        binary_links: links,
    })
}

pub fn validate_registered_repository_configs(
    database_path: &Path,
    control_binary: &Path,
    runner: &dyn CommandRunner,
) -> Result<Vec<RepositoryConfigReceipt>, String> {
    validate_regular(control_binary, true)
        .map_err(|error| format!("Rust control binary is unavailable: {error}"))?;
    let connection = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .map_err(|error| format!("cannot open registered repository database: {error}"))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| format!("cannot protect registered repository query: {error}"))?;
    let mut statement = connection
        .prepare(
            "SELECT w.worktree_path FROM worktrees w JOIN repositories r ON r.repository_id=w.repository_id WHERE r.archived_at IS NULL ORDER BY w.worktree_path",
        )
        .map_err(|error| format!("cannot inventory registered repository configs: {error}"))?;
    let worktrees = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("cannot inventory registered repository configs: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot inventory registered repository configs: {error}"))?;
    drop(statement);
    let mut receipts = Vec::new();
    for worktree in worktrees {
        let worktree = PathBuf::from(worktree);
        let config = worktree.join(".devcoordinator.toml");
        let metadata = match config.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect registered repository config: {error}"
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("registered repository config is not a regular file".to_owned());
        }
        let output = runner.run(&CommandRequest {
            program: control_binary.to_owned(),
            args: vec![
                "--validate-repository-config".into(),
                worktree.as_os_str().to_owned(),
            ],
            environment: BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            clear_environment: true,
        })?;
        if !output.success {
            return Err(format!(
                "registered repository is not ready for strict schema 2: {}",
                useful_failure(&output)
            ));
        }
        if output.stdout_truncated {
            return Err("repository config validator output exceeded 64 KiB".to_owned());
        }
        let value: Value = serde_json::from_str(&output.stdout).map_err(|error| {
            format!("repository config validator returned invalid JSON: {error}")
        })?;
        if value.get("schema").and_then(Value::as_u64) != Some(2) {
            return Err("repository config validator returned an unsupported schema".to_owned());
        }
        let tests = string_array(value.get("tests"), "tests")?;
        let deployments = string_array(value.get("deployments"), "deployments")?;
        receipts.push(RepositoryConfigReceipt {
            worktree: path_text(&worktree)?,
            tests,
            deployments,
        });
    }
    Ok(receipts)
}

pub fn render_daemon_unit(source_root: &Path) -> Result<String, String> {
    let template = read_template(&source_root.join("deploy/devcoordinator2.service"))?;
    let binary = path_text(&source_root.join("target/release/devcoordinator2"))?;
    let rendered = template.replace(
        "/home/DevCoordinator2/target/release/devcoordinator2",
        &binary,
    );
    if rendered.contains("python") || !rendered.contains(&format!("ExecStart={binary} daemon")) {
        return Err("daemon unit does not select the verified Rust binary".to_owned());
    }
    Ok(rendered)
}

pub fn render_edge_unit(source_root: &Path, canary: bool) -> Result<String, String> {
    let template = read_template(&source_root.join("deploy/devcoordinator2-edge.service"))?;
    let script = path_text(&source_root.join("edge/devcoordinator2-edge.mjs"))?;
    let mut rendered = template.replace(
        "/home/DevCoordinator2/edge/devcoordinator2-edge.mjs",
        &script,
    );
    rendered = rendered.replace(
        "ReadOnlyPaths=/home/DevCoordinator2",
        &format!("ReadOnlyPaths={}", path_text(source_root)?),
    );
    if canary {
        rendered = rendered
            .lines()
            .filter(|line| {
                ![
                    "LoadCredential=tls",
                    "LoadCredential=oidc",
                    "Environment=EDGE_TLS_CERT",
                    "Environment=EDGE_OIDC_CLIENT",
                    "AmbientCapabilities",
                    "CapabilityBoundingSet",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            })
            .collect::<Vec<_>>()
            .join("\n");
        rendered.push('\n');
    }
    if !rendered.contains(&format!("ExecStart=/usr/bin/node {script}")) {
        return Err("edge unit does not select the canonical Node source".to_owned());
    }
    Ok(rendered)
}

pub fn replace_direct_link(destination: &Path, source: &Path) -> Result<(), String> {
    if destination
        .symlink_metadata()
        .is_ok_and(|metadata| !metadata.file_type().is_symlink())
    {
        return Err(format!(
            "refusing to replace non-symlink managed path: {}",
            destination.display()
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "managed link has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create managed link directory: {error}"))?;
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "managed link name is invalid".to_owned())?;
    let temporary = parent.join(format!(".{name}.tmp"));
    if let Ok(metadata) = temporary.symlink_metadata() {
        if !metadata.file_type().is_symlink() {
            return Err("refusing to replace non-symlink staging path".to_owned());
        }
        std::fs::remove_file(&temporary)
            .map_err(|error| format!("cannot clear managed link staging path: {error}"))?;
    }
    symlink(source, &temporary).map_err(|error| format!("cannot create managed link: {error}"))?;
    std::fs::rename(&temporary, destination)
        .map_err(|error| format!("cannot activate managed link: {error}"))?;
    Ok(())
}

pub fn write_if_absent(
    path: &Path,
    content: &str,
    mode: u32,
    owner: (u32, u32),
) -> Result<bool, String> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("refusing non-regular existing installation target".to_owned())
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            atomic_file(path, content.as_bytes(), mode, Some(owner))?;
            Ok(true)
        }
        Err(error) => Err(format!("cannot inspect installation target: {error}")),
    }
}

pub fn ensure_env_value(path: &Path, key: &str, value: &str) -> Result<bool, String> {
    validate_env_pair(key, value)?;
    let (text, metadata) = read_optional_regular(path)?;
    let prefix = format!("{key}=");
    let matches = text
        .lines()
        .filter(|line| line.trim().starts_with(&prefix))
        .collect::<Vec<_>>();
    let desired = format!("{key}={value}");
    if !matches.is_empty() {
        if matches != [desired.as_str()] {
            return Err(format!("{key} already has a different installed value"));
        }
        return Ok(false);
    }
    let suffix = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let updated = format!("{text}{suffix}{desired}\n");
    let mode = metadata
        .as_ref()
        .map_or(0o640, |metadata| metadata.mode() & 0o777);
    let owner = metadata
        .as_ref()
        .map(|metadata| (metadata.uid(), metadata.gid()));
    atomic_file(path, updated.as_bytes(), mode, owner)?;
    Ok(true)
}

pub fn set_env_value(path: &Path, key: &str, value: &str) -> Result<bool, String> {
    validate_env_pair(key, value)?;
    let (text, metadata) = read_optional_regular(path)?;
    let prefix = format!("{key}=");
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let indexes = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim().starts_with(&prefix))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if indexes.len() > 1 {
        return Err(format!("{key} appears more than once"));
    }
    let desired = format!("{key}={value}");
    if indexes
        .first()
        .is_some_and(|index| lines[*index] == desired)
    {
        return Ok(false);
    }
    if let Some(index) = indexes.first() {
        lines[*index] = desired;
    } else {
        lines.push(desired);
    }
    let updated = format!("{}\n", lines.join("\n"));
    let mode = metadata
        .as_ref()
        .map_or(0o640, |metadata| metadata.mode() & 0o777);
    let owner = metadata
        .as_ref()
        .map(|metadata| (metadata.uid(), metadata.gid()));
    atomic_file(path, updated.as_bytes(), mode, owner)?;
    Ok(true)
}

pub fn compose_env_authorizations<R: CommandRunner>(
    specifications: &[String],
    runner: &R,
) -> Result<Vec<ComposeAuthorization>, String> {
    let mut entries = BTreeSet::new();
    for specification in specifications {
        let (repository_text, relative) = specification.split_once('=').ok_or_else(|| {
            "--compose-env-authorization must be REPOSITORY=RELATIVE_PATH".to_owned()
        })?;
        if repository_text.is_empty() || relative.is_empty() {
            return Err("--compose-env-authorization must be REPOSITORY=RELATIVE_PATH".to_owned());
        }
        validate_relative_file(relative)?;
        let repository = Path::new(repository_text)
            .canonicalize()
            .map_err(|error| format!("cannot resolve Compose repository: {error}"))?;
        let identity = git(
            &repository,
            [
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-common-dir",
            ],
            runner,
        )?;
        let lines = identity.lines().collect::<Vec<_>>();
        if lines.len() != 2 || Path::new(lines[1]).file_name() != Some(OsStr::new(".git")) {
            return Err("unsupported repository layout".to_owned());
        }
        let worktree = Path::new(lines[0])
            .canonicalize()
            .map_err(|error| format!("cannot resolve worktree: {error}"))?;
        let common_root = Path::new(lines[1])
            .canonicalize()
            .map_err(|error| format!("cannot resolve Git common directory: {error}"))?
            .parent()
            .ok_or_else(|| "Git common directory has no repository root".to_owned())?
            .to_owned();
        let candidate = worktree.join(relative);
        let metadata = candidate
            .symlink_metadata()
            .map_err(|error| format!("cannot inspect Compose environment file: {error}"))?;
        let resolved = candidate
            .canonicalize()
            .map_err(|error| format!("cannot resolve Compose environment file: {error}"))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || (resolved != worktree && !resolved.starts_with(&worktree))
        {
            return Err("unsafe Compose environment file".to_owned());
        }
        let ignored = run_git_raw(
            &worktree,
            ["check-ignore", "--quiet", "--", relative],
            runner,
        )?;
        if !ignored.success {
            return Err("Compose environment file must be ignored".to_owned());
        }
        let mut digest = Sha256::new();
        digest.update(b"devcoordinator2.repository\0");
        digest.update(common_root.as_os_str().as_encoded_bytes());
        let entry = ComposeAuthorization {
            repository_id: format!("r{}", &hex(&digest.finalize())[..16]),
            path: relative.to_owned(),
        };
        if !entries.insert(entry) {
            return Err("duplicate Compose environment authorization".to_owned());
        }
    }
    Ok(entries.into_iter().collect())
}

pub fn merge_compose_env_allowlist(
    path: &Path,
    additions: &[ComposeAuthorization],
    owner: (u32, u32),
) -> Result<bool, String> {
    let existing = match read_optional_regular(path)? {
        (text, Some(_)) => {
            let policy: ComposePolicy = serde_json::from_str(&text).map_err(|error| {
                format!("cannot preserve Compose environment allowlist: {error}")
            })?;
            if policy.schema != 1 {
                return Err(
                    "cannot preserve Compose environment allowlist: wrong schema".to_owned(),
                );
            }
            policy.authorizations
        }
        (_, None) => Vec::new(),
    };
    let merged = existing
        .into_iter()
        .chain(additions.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let policy = ComposePolicy {
        schema: 1,
        authorizations: merged,
    };
    merge_policy(path, &policy, 0o640, owner)
}

pub fn codex_usage_sources<F>(
    accounts: &[String],
    mut resolve: F,
) -> Result<Vec<CodexUsageSource>, String>
where
    F: FnMut(&str) -> Result<LocalAccount, String>,
{
    let mut sources = BTreeSet::new();
    for name in accounts {
        let account = resolve(name)?;
        let codex_home = account.home.join(".codex");
        let executable = account.home.join(".local/bin/codex");
        let executable_ok = executable
            .symlink_metadata()
            .is_ok_and(|metadata| !metadata.file_type().is_symlink() && metadata.is_file());
        if !codex_home.is_dir() || !executable_ok {
            return Err(format!("Codex usage source is not installed for {name}"));
        }
        sources.insert(CodexUsageSource {
            uid: account.uid,
            codex_home: path_text(&codex_home)?,
            executable: path_text(&executable)?,
        });
    }
    Ok(sources.into_iter().collect())
}

pub fn merge_codex_usage_sources(
    path: &Path,
    additions: &[CodexUsageSource],
    owner: (u32, u32),
) -> Result<bool, String> {
    let existing = match read_optional_regular(path)? {
        (text, Some(_)) => {
            let policy: UsagePolicy = serde_json::from_str(&text)
                .map_err(|error| format!("cannot preserve Codex usage source policy: {error}"))?;
            if policy.schema != 1 {
                return Err("cannot preserve Codex usage source policy: wrong schema".to_owned());
            }
            policy.sources
        }
        (_, None) => Vec::new(),
    };
    let mut by_uid = BTreeMap::new();
    for source in existing.into_iter().chain(additions.iter().cloned()) {
        by_uid.insert(source.uid, source);
    }
    let policy = UsagePolicy {
        schema: 1,
        sources: by_uid.into_values().collect(),
    };
    merge_policy(path, &policy, 0o600, owner)
}

pub fn account_by_name(name: &str) -> Result<LocalAccount, String> {
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|error| format!("cannot read local account registry: {error}"))?;
    for line in passwd.lines() {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() >= 7 && fields[0] == name {
            return Ok(LocalAccount {
                uid: fields[2]
                    .parse()
                    .map_err(|_| format!("local account {name} has an invalid uid"))?,
                gid: fields[3]
                    .parse()
                    .map_err(|_| format!("local account {name} has an invalid gid"))?,
                home: PathBuf::from(fields[5]),
            });
        }
    }
    Err(format!("local account {name} does not exist"))
}

pub fn ensure_identities<R: CommandRunner, D: IdentityDirectory>(
    client_group: &str,
    client_accounts: &[String],
    edge_account_name: &str,
    runner: &R,
    directory: &D,
) -> Result<(IdentityReceipt, LocalAccount), String> {
    validate_account_name(client_group, "client group")?;
    validate_account_name(edge_account_name, "edge account")?;
    let client_gid = match directory.group_gid(client_group)? {
        Some(gid) => gid,
        None => {
            run_required(
                runner,
                Path::new("/usr/sbin/groupadd"),
                &["--system", client_group],
                "create client group",
            )?;
            directory
                .group_gid(client_group)?
                .ok_or_else(|| "client group was not created".to_owned())?
        }
    };
    let mut seen = BTreeSet::new();
    for name in client_accounts {
        validate_account_name(name, "client account")?;
        if !seen.insert(name.clone()) {
            return Err(format!("duplicate client account {name}"));
        }
        directory
            .account(name)?
            .ok_or_else(|| format!("local account {name} does not exist"))?;
        run_required(
            runner,
            Path::new("/usr/sbin/usermod"),
            &["-aG", client_group, name],
            "add client account to group",
        )?;
    }
    let edge = match directory.account(edge_account_name)? {
        Some(account) => {
            run_required(
                runner,
                Path::new("/usr/sbin/usermod"),
                &["-aG", client_group, edge_account_name],
                "add edge account to client group",
            )?;
            account
        }
        None => {
            run_required(
                runner,
                Path::new("/usr/sbin/useradd"),
                &[
                    "--system",
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    "--home-dir",
                    "/var/lib/devcoordinator2-edge",
                    "-G",
                    client_group,
                    edge_account_name,
                ],
                "create edge account",
            )?;
            directory
                .account(edge_account_name)?
                .ok_or_else(|| "edge account was not created".to_owned())?
        }
    };
    Ok((
        IdentityReceipt {
            client_group: client_group.to_owned(),
            client_gid,
            edge_uid: edge.uid,
            edge_gid: edge.gid,
            client_accounts: seen.into_iter().collect(),
        },
        edge,
    ))
}

pub fn prepare_instance<R: CommandRunner>(
    request: &PrepareRequest,
    runner: &R,
) -> Result<PrepareReceipt, String> {
    let source_root = request
        .source_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve canonical source: {error}"))?;
    validate_domain(&request.base_domain)?;
    if request.admin_emails.trim().is_empty()
        || request
            .admin_emails
            .chars()
            .any(|character| character.is_control())
    {
        return Err("administrator e-mail list is invalid".to_owned());
    }
    let acl_count = ensure_edge_source_access(&source_root, "devcoordinator2-edge", runner)?;
    ensure_directory(&request.paths.runtime_dir, 0o755, request.system_owner)?;
    prepare_state_directory(&request.paths.state_dir, request.system_owner)?;
    ensure_directory(
        &request.paths.state_dir.join("public"),
        0o755,
        request.system_owner,
    )?;
    ensure_directory(
        &request.paths.bugs_dir,
        0o777,
        (request.system_owner.0, request.identities.client_gid),
    )?;
    ensure_directory(
        &request.paths.edge_state_dir,
        0o750,
        (request.edge_account.uid, request.edge_account.gid),
    )?;
    let tmpfiles = read_template(&source_root.join("deploy/devcoordinator2.tmpfiles.conf"))?;
    atomic_file(
        &request.paths.tmpfiles_target,
        tmpfiles.as_bytes(),
        0o644,
        Some(request.system_owner),
    )?;

    ensure_directory(&request.paths.etc_dir, 0o755, request.system_owner)?;
    let instance_env = request.paths.etc_dir.join("instance.env");
    let created_instance_env = write_if_absent(
        &instance_env,
        &format!(
            "DEVCOORDINATOR2_BASE_DOMAIN={}\nDEVCOORDINATOR2_ADMIN_EMAILS={}\nDEVCOORDINATOR2_CLIENT_GROUP={}\nDEVCOORDINATOR2_EDGE_UID={}\nDEVCOORDINATOR2_PORT_RANGE=20000-29999\n# DEVCOORDINATOR2_TELEGRAM_TOKEN_FILE=/etc/devcoordinator2/telegram.token\n",
            request.base_domain,
            request.admin_emails,
            request.identities.client_group,
            request.edge_account.uid,
        ),
        0o640,
        (request.system_owner.0, request.identities.client_gid),
    )?;
    let compose_path = request.paths.etc_dir.join("compose-env-allowlist.json");
    let compose_policy_changed =
        if !request.compose_authorizations.is_empty() || compose_path.exists() {
            let changed = merge_compose_env_allowlist(
                &compose_path,
                &request.compose_authorizations,
                request.system_owner,
            )?;
            ensure_env_value(
                &instance_env,
                "DEVCOORDINATOR2_COMPOSE_ENV_ALLOWLIST_FILE",
                &path_text(&compose_path)?,
            )?;
            changed
        } else {
            false
        };
    let usage_path = request.paths.etc_dir.join("codex-usage-sources.json");
    let usage_policy_changed = if !request.usage_sources.is_empty() || usage_path.exists() {
        let changed =
            merge_codex_usage_sources(&usage_path, &request.usage_sources, request.system_owner)?;
        ensure_env_value(
            &instance_env,
            "DEVCOORDINATOR2_CODEX_USAGE_SOURCES_FILE",
            &path_text(&usage_path)?,
        )?;
        changed
    } else {
        false
    };

    let edge_env = request.paths.etc_dir.join("edge.env");
    let mut edge_lines = vec![
        format!("EDGE_BASE_DOMAIN={}", request.base_domain),
        format!(
            "EDGE_ROUTES_FILE={}",
            request.paths.state_dir.join("public/routes.json").display()
        ),
        format!(
            "EDGE_DAEMON_SOCKET={}",
            request.paths.runtime_dir.join("daemon.sock").display()
        ),
        format!("EDGE_CONSOLE_DIR={}", source_root.join("console").display()),
    ];
    if request.canary {
        edge_lines.extend([
            "EDGE_HTTP_ONLY=1".to_owned(),
            format!("EDGE_HTTP_PORT={}", request.canary_port),
            format!(
                "EDGE_SESSION_SECRET_FILE={}",
                request.paths.etc_dir.join("edge/session.secret").display()
            ),
            "# OIDC for the canary: set client ID and secret files after registering its redirect URI".to_owned(),
        ]);
    }
    let created_edge_env = write_if_absent(
        &edge_env,
        &(edge_lines.join("\n") + "\n"),
        0o640,
        (request.system_owner.0, request.edge_account.gid),
    )?;
    set_env_value(
        &edge_env,
        "EDGE_CONSOLE_DIR",
        &path_text(&source_root.join("console"))?,
    )?;
    let edge_secrets = request.paths.etc_dir.join("edge");
    ensure_directory(
        &edge_secrets,
        0o750,
        (request.system_owner.0, request.edge_account.gid),
    )?;
    let mut secret = [0u8; 48];
    getrandom::fill(&mut secret)
        .map_err(|error| format!("cannot generate edge session secret: {error}"))?;
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let created_session_secret = write_if_absent(
        &edge_secrets.join("session.secret"),
        &secret,
        0o640,
        (request.system_owner.0, request.edge_account.gid),
    )?;
    Ok(PrepareReceipt {
        source_root: path_text(&source_root)?,
        created_instance_env,
        created_edge_env,
        created_session_secret,
        compose_policy_changed,
        usage_policy_changed,
        edge_source_acl_entries: u32::try_from(acl_count).unwrap_or(u32::MAX),
    })
}

/// Protect the existing private leaves before making published state traversable.
/// The public edge is untrusted; owner-controlled local accounts remain trusted.
pub(crate) fn prepare_state_directory(path: &Path, owner: (u32, u32)) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    let open = |path: &Path, directory: bool| -> Result<File, String> {
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        let file = unix_open(
            path,
            if directory {
                flags | OFlags::DIRECTORY
            } else {
                flags
            },
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| error.to_string())?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if (metadata.uid(), metadata.gid()) != owner || (!directory && !metadata.is_file()) {
            return Err(
                "state permission repair requires owned regular files and directories".to_owned(),
            );
        }
        Ok(file)
    };
    let root = open(path, true)?;
    let mut private = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("unrecognized state entry")?;
        match name {
            "public" | "deployments" | "secrets" => {
                open(&entry.path(), true)?;
            }
            "cutover" | "recovery" => private.push((open(&entry.path(), true)?, 0o700)),
            name if name == "authority.sqlite3"
                || name.starts_with("authority.sqlite3-")
                || name.starts_with("authority.sqlite3.") =>
            {
                private.push((open(&entry.path(), false)?, 0o600));
            }
            _ => {
                return Err(
                    "unrecognized state entry; inspect it before changing traversal".to_owned(),
                );
            }
        }
    }
    // Preflight every entry before any permission change. Preserve data and owners.
    for (file, mode) in private {
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
    }
    root.set_permissions(std::fs::Permissions::from_mode(0o751))
        .map_err(|error| error.to_string())
}

/// Update only our state-root rule, retaining instance-specific sibling rules.
pub(crate) fn state_tmpfiles(source_root: &Path, installed: &Path) -> Result<String, String> {
    let template = read_template(&source_root.join("deploy/devcoordinator2.tmpfiles.conf"))?;
    let rule = template
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some("/var/lib/devcoordinator2"))
        .ok_or("source state-directory rule is missing")?;
    let (current, metadata) = read_optional_regular(installed)?;
    if metadata.is_none() {
        return Ok(template);
    }
    let mut found = false;
    let mut result = String::new();
    for line in current.lines() {
        if line.split_whitespace().nth(1) == Some("/var/lib/devcoordinator2") {
            if found {
                return Err("duplicate state-directory rule".to_owned());
            }
            found = true;
            result.push_str(rule);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    if !found {
        result.push_str(rule);
        result.push('\n');
    }
    Ok(result)
}

pub fn ensure_edge_source_access<R: CommandRunner>(
    source_root: &Path,
    edge_account: &str,
    runner: &R,
) -> Result<usize, String> {
    validate_account_name(edge_account, "edge account")?;
    let source_root = source_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve source root for edge access: {error}"))?;
    let mut commands = vec![(
        vec![
            "-m".to_owned(),
            format!("u:{edge_account}:--x"),
            path_text(&source_root)?,
        ],
        "grant edge source-root traversal",
    )];
    for tree_name in ["edge", "console"] {
        let tree = source_root.join(tree_name);
        let metadata = tree
            .symlink_metadata()
            .map_err(|error| format!("required edge source tree is unavailable: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("required edge source tree is unavailable".to_owned());
        }
        let mut pending = vec![tree];
        while let Some(directory) = pending.pop() {
            commands.push((
                vec![
                    "-m".to_owned(),
                    format!("u:{edge_account}:r-x"),
                    path_text(&directory)?,
                ],
                "grant edge source directory access",
            ));
            commands.push((
                vec![
                    "-m".to_owned(),
                    format!("d:u:{edge_account}:r-x"),
                    path_text(&directory)?,
                ],
                "grant edge default source directory access",
            ));
            let mut entries = std::fs::read_dir(&directory)
                .map_err(|error| format!("cannot inventory edge source tree: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("cannot inventory edge source tree: {error}"))?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries.into_iter().rev() {
                let metadata = entry
                    .path()
                    .symlink_metadata()
                    .map_err(|error| format!("cannot inspect edge source entry: {error}"))?;
                if metadata.file_type().is_symlink() {
                    return Err("edge source tree may not contain symlinks".to_owned());
                }
                if metadata.is_dir() {
                    pending.push(entry.path());
                } else if metadata.is_file() {
                    commands.push((
                        vec![
                            "-m".to_owned(),
                            format!("u:{edge_account}:r--"),
                            path_text(&entry.path())?,
                        ],
                        "grant edge source file access",
                    ));
                } else {
                    return Err("edge source tree contains a special file".to_owned());
                }
            }
        }
    }
    for (arguments, label) in &commands {
        let refs = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        run_required(runner, Path::new("/usr/bin/setfacl"), &refs, label)?;
    }
    Ok(commands.len())
}

pub fn ensure_owned_directory(path: &Path, mode: u32, owner: (u32, u32)) -> Result<(), String> {
    ensure_directory(path, mode, owner)
}

pub fn retire_legacy_skill_link(path: &Path) -> Result<bool, String> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect legacy skill link: {error}")),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let target = std::fs::read_link(path)
        .map_err(|error| format!("cannot read legacy skill link: {error}"))?;
    let components = target.components().collect::<Vec<_>>();
    let managed = components.len() >= 2
        && components[components.len() - 2].as_os_str() == OsStr::new("skills")
        && components[components.len() - 1].as_os_str() == OsStr::new("codex-dev-coordinator");
    if !managed {
        return Ok(false);
    }
    std::fs::remove_file(path)
        .map_err(|error| format!("cannot retire legacy skill link: {error}"))?;
    Ok(true)
}

fn git<R, I, S>(root: &Path, args: I, runner: &R) -> Result<String, String>
where
    R: CommandRunner,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_git_raw(root, args, runner)?;
    if !output.success {
        return Err(format!("Git preflight failed: {}", useful_failure(&output)));
    }
    if output.stdout_truncated {
        return Err("Git preflight output exceeded 64 KiB".to_owned());
    }
    Ok(output.stdout)
}

fn run_git_raw<R, I, S>(root: &Path, args: I, runner: &R) -> Result<CommandOutput, String>
where
    R: CommandRunner,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command_args = vec![
        "-c".into(),
        "safe.directory=*".into(),
        "-C".into(),
        root.as_os_str().to_owned(),
    ];
    command_args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    runner.run(&CommandRequest {
        program: PathBuf::from("/usr/bin/git"),
        args: command_args,
        environment: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        clear_environment: true,
    })
}

fn validate_env_pair(key: &str, value: &str) -> Result<(), String> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || value.contains(['\n', '\r', '\0'])
    {
        return Err("environment entry is invalid".to_owned());
    }
    Ok(())
}

fn validate_account_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return Err(format!("{label} name is invalid"));
    }
    Ok(())
}

fn validate_domain(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 253
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err("base domain is invalid".to_owned());
    }
    Ok(())
}

fn group_by_name(name: &str) -> Result<Option<u32>, String> {
    let groups = std::fs::read_to_string("/etc/group")
        .map_err(|error| format!("cannot read local group registry: {error}"))?;
    for line in groups.lines() {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() >= 4 && fields[0] == name {
            return fields[2]
                .parse::<u32>()
                .map(Some)
                .map_err(|_| format!("local group {name} has an invalid gid"));
        }
    }
    Ok(None)
}

fn run_required<R: CommandRunner>(
    runner: &R,
    program: &Path,
    arguments: &[&str],
    label: &str,
) -> Result<(), String> {
    let output = runner.run(&CommandRequest {
        program: program.to_owned(),
        args: arguments.iter().map(OsString::from).collect(),
        environment: BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/sbin:/usr/bin:/sbin:/bin"),
        )]),
        clear_environment: true,
    })?;
    if output.success {
        Ok(())
    } else {
        Err(format!("{label} failed: {}", useful_failure(&output)))
    }
}

fn ensure_directory(path: &Path, mode: u32, owner: (u32, u32)) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("cannot create installation directory: {error}"))?;
    let descriptor = unix_open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open installation directory: {error}"))?;
    descriptor
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot set installation directory mode: {error}"))?;
    let metadata = descriptor
        .metadata()
        .map_err(|error| format!("cannot inspect installation directory: {error}"))?;
    if (metadata.uid(), metadata.gid()) != owner
        // SAFETY: `descriptor` owns a valid open directory for the call.
        && unsafe { libc::fchown(descriptor.as_raw_fd(), owner.0, owner.1) } != 0
    {
        return Err(format!(
            "cannot set installation directory owner: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn string_array(value: Option<&Value>, label: &str) -> Result<Vec<String>, String> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("repository config validator has no {label} array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("repository config validator {label} is invalid"))
        })
        .collect()
}

fn validate_relative_file(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains(['\\', '\0'])
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(
            "Compose environment authorization path must be normalized relative".to_owned(),
        );
    }
    Ok(())
}

fn read_optional_regular(path: &Path) -> Result<(String, Option<std::fs::Metadata>), String> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), None));
        }
        Err(error) => return Err(format!("cannot inspect installed policy: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 256 * 1024 {
        return Err("installed policy must be a bounded regular non-symlink file".to_owned());
    }
    let file = unix_open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open installed policy: {error}"))?;
    let mut text = String::new();
    file.take(256 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read installed policy: {error}"))?;
    if text.len() > 256 * 1024 {
        return Err("installed policy exceeds 256 KiB".to_owned());
    }
    Ok((text, Some(metadata)))
}

fn merge_policy<T: Serialize>(
    path: &Path,
    policy: &T,
    mode: u32,
    owner: (u32, u32),
) -> Result<bool, String> {
    let mut payload = serde_json::to_vec_pretty(policy)
        .map_err(|error| format!("cannot encode installed policy: {error}"))?;
    payload.push(b'\n');
    if let (text, Some(metadata)) = read_optional_regular(path)?
        && serde_json::from_str::<Value>(&text).ok()
            == serde_json::from_slice::<Value>(&payload).ok()
    {
        let changed = metadata.uid() != owner.0
            || metadata.gid() != owner.1
            || metadata.mode() & 0o777 != mode;
        if changed {
            let file = unix_open(
                path,
                OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| format!("cannot open installed policy metadata: {error}"))?;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|error| format!("cannot set installed policy mode: {error}"))?;
            if (metadata.uid(), metadata.gid()) != owner
                // SAFETY: `file` owns a valid descriptor for the complete call.
                && unsafe {
                    libc::fchown(
                        std::os::fd::AsRawFd::as_raw_fd(&file),
                        owner.0,
                        owner.1,
                    )
                } != 0
            {
                return Err(format!(
                    "cannot set installed policy owner: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        return Ok(changed);
    }
    atomic_file(path, &payload, mode, Some(owner))?;
    Ok(true)
}

fn validate_tool(path: &Path, name: &str) -> Result<(), String> {
    validate_regular(path, true)
        .map(|_| ())
        .map_err(|error| format!("required {name} tool is unavailable: {error}"))
}

fn validate_regular(path: &Path, executable: bool) -> Result<std::fs::Metadata, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular non-symlink file",
            path.display()
        ));
    }
    if executable && metadata.mode() & 0o100 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(metadata)
}

fn read_template(path: &Path) -> Result<String, String> {
    validate_regular(path, false)?;
    let file = unix_open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open unit template: {error}"))?;
    let mut text = String::new();
    file.take(TEMPLATE_CAP + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read unit template: {error}"))?;
    if text.len() as u64 > TEMPLATE_CAP {
        return Err("unit template exceeds 256 KiB".to_owned());
    }
    Ok(text)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = unix_open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open release binary for hashing: {error}"))?;
    let before = file
        .metadata()
        .map_err(|error| format!("cannot inspect release binary: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash release binary: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("cannot recheck release binary: {error}"))?;
    if file_identity(&before) != file_identity(&after) {
        return Err("release binary changed while it was hashed".to_owned());
    }
    Ok(hex(&digest.finalize()))
}

pub(crate) fn atomic_file(
    path: &Path,
    payload: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "installation target has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create installation directory: {error}"))?;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random)
        .map_err(|error| format!("cannot create installation temporary identity: {error}"))?;
    let temporary = parent.join(format!(".install-{}.tmp", hex(&random)));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("cannot create installation temporary: {error}"))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot set installation mode: {error}"))?;
        if let Some((uid, gid)) = owner
            && file
                .metadata()
                .map(|metadata| (metadata.uid(), metadata.gid()) != (uid, gid))
                .unwrap_or(true)
        {
            // SAFETY: `file` owns a valid descriptor for the complete call.
            if unsafe { libc::fchown(std::os::fd::AsRawFd::as_raw_fd(&file), uid, gid) } != 0 {
                return Err(format!(
                    "cannot set installation owner: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        file.write_all(payload)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("cannot persist installation file: {error}"))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("cannot activate installation file: {error}"))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("cannot sync installation directory: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn passwd_identity(uid: u32) -> Result<(u32, PathBuf), String> {
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|error| format!("cannot read local account registry: {error}"))?;
    for line in passwd.lines() {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid) {
            let gid = fields[3]
                .parse::<u32>()
                .map_err(|_| "build account group is invalid".to_owned())?;
            return Ok((gid, PathBuf::from(fields[5])));
        }
    }
    Err("Rust Cargo manifest has no valid non-root owner".to_owned())
}

fn validate_commit(value: &str) -> Result<(), String> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("source commit must be a complete lowercase Git object ID".to_owned())
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "path must be normalized and absolute: {}",
            path.display()
        ));
    }
    path.as_os_str()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "installation paths must be valid UTF-8".to_owned())
}

fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn read_bounded_stream(mut stream: impl Read) -> Result<(String, bool), String> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut block = [0u8; 16 * 1024];
    loop {
        let count = stream
            .read(&mut block)
            .map_err(|error| format!("cannot read command output: {error}"))?;
        if count == 0 {
            break;
        }
        retained.extend_from_slice(&block[..count]);
        if retained.len() > OUTPUT_CAP {
            truncated = true;
            let excess = retained.len() - OUTPUT_CAP;
            retained.drain(..excess);
        }
    }
    Ok((String::from_utf8_lossy(&retained).into_owned(), truncated))
}

fn useful_failure(output: &CommandOutput) -> String {
    let value = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if value.is_empty() {
        "command exited nonzero".to_owned()
    } else {
        value
            .chars()
            .rev()
            .take(2_000)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(value, "{byte:02x}").expect("string write");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeRunner {
        requests: Arc<Mutex<Vec<CommandRequest>>>,
        outputs: Arc<Mutex<VecDeque<CommandOutput>>>,
    }

    impl FakeRunner {
        fn with_outputs(outputs: Vec<CommandOutput>) -> Self {
            Self {
                requests: Arc::default(),
                outputs: Arc::new(Mutex::new(outputs.into())),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, String> {
            self.requests.lock().unwrap().push(request.clone());
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "unexpected command".to_owned())
        }
    }

    #[derive(Default)]
    struct AlwaysRunner {
        requests: Mutex<Vec<CommandRequest>>,
    }

    impl CommandRunner for AlwaysRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, String> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(success(""))
        }
    }

    struct StaticIdentities {
        gid: u32,
        accounts: BTreeMap<String, LocalAccount>,
    }

    impl IdentityDirectory for StaticIdentities {
        fn group_gid(&self, _name: &str) -> Result<Option<u32>, String> {
            Ok(Some(self.gid))
        }

        fn account(&self, name: &str) -> Result<Option<LocalAccount>, String> {
            Ok(self.accounts.get(name).cloned())
        }
    }

    fn success(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: stdout.to_owned(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn live_checkout_requires_clean_current_main_at_upstream() {
        let temporary = tempfile::tempdir().unwrap();
        let remote = temporary.path().join("remote.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        let root = temporary.path().join("live");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        for args in [
            vec!["config", "user.name", "fixture"],
            vec!["config", "user.email", "fixture@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        for args in [
            vec!["add", "tracked.txt"],
            vec!["commit", "-q", "-m", "initial"],
            vec!["branch", "-M", "main"],
            vec!["remote", "add", "origin", remote.to_str().unwrap()],
            vec!["push", "-q", "-u", "origin", "main"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let head = validate_live_checkout(&root, true, &HostRunner).unwrap();
        assert!(matches!(head.len(), 40 | 64));
        std::fs::write(root.join("dirty.txt"), "dirty").unwrap();
        assert!(
            validate_live_checkout(&root, false, &HostRunner)
                .unwrap_err()
                .contains("must be clean")
        );
    }

    #[test]
    fn rendered_daemon_keeps_policy_write_exception_narrow() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path();
        std::fs::create_dir(source.join("deploy")).unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.service"),
            include_str!("../../../deploy/devcoordinator2.service"),
        )
        .unwrap();
        let rendered = render_daemon_unit(source).unwrap();
        assert!(rendered.lines().any(|line| line == "ProtectSystem=full"));
        assert!(rendered.lines().any(|line| line == "NoNewPrivileges=yes"));
        let paths = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("ReadWritePaths="))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["/home /var/lib/devcoordinator2 /run/devcoordinator2 /etc/devcoordinator2"]
        );
    }

    #[test]
    fn build_runs_as_checkout_owner_and_verifies_every_binary_commit() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path();
        std::fs::write(source.join("Cargo.toml"), "[workspace]\n").unwrap();
        let tools = BuildTools {
            cargo: source.join("cargo"),
            setpriv: source.join("setpriv"),
        };
        for tool in [&tools.cargo, &tools.setpriv] {
            std::fs::write(tool, "tool").unwrap();
            std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let commit = "a".repeat(40);
        let release = source.join("target/release");
        std::fs::create_dir_all(&release).unwrap();
        for name in BINARY_NAMES {
            let path = release.join(name);
            std::fs::write(&path, name).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = FakeRunner::with_outputs(
            std::iter::once(success(""))
                .chain(BINARY_NAMES.iter().map(|_| success(&commit)))
                .collect(),
        );
        let identity = BuildIdentity {
            uid: 1000,
            gid: 1000,
            home: source.to_owned(),
        };
        let receipts = build_release_binaries(source, &commit, &identity, &tools, &runner).unwrap();
        assert_eq!(receipts.len(), 3);
        let requests = runner.requests.lock().unwrap();
        assert_eq!(requests[0].program, tools.setpriv);
        assert!(requests[0].args.contains(&OsString::from("--workspace")));
        assert!(requests[0].args.contains(&OsString::from(format!(
            "DEVCOORDINATOR2_SOURCE_COMMIT={commit}"
        ))));
        assert!(
            requests[1..]
                .iter()
                .all(|request| request.args == [OsString::from("--source-commit")])
        );
    }

    #[test]
    fn manifest_hash_unit_and_direct_links_are_exact() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir_all(source.join("deploy")).unwrap();
        std::fs::create_dir_all(source.join("edge")).unwrap();
        std::fs::create_dir_all(source.join("target/release")).unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.service"),
            "[Service]\nExecStart=/home/DevCoordinator2/target/release/devcoordinator2 daemon\n",
        )
        .unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2-edge.service"),
            "[Service]\nExecStart=/usr/bin/node /home/DevCoordinator2/edge/devcoordinator2-edge.mjs\nProtectHome=read-only\nReadOnlyPaths=/home/DevCoordinator2\n",
        )
        .unwrap();
        let commit = "b".repeat(40);
        let mut outputs = Vec::new();
        for name in BINARY_NAMES {
            let path = source.join("target/release").join(name);
            std::fs::write(&path, name).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            outputs.push(success(&commit));
        }
        let runner = FakeRunner::with_outputs(outputs);
        let binaries = verify_release_binaries(&source, &commit, &runner).unwrap();
        let manifest = manifest(&source, &commit, "2026-09-04T00:00:00Z", binaries).unwrap();
        let manifest_path = temporary.path().join("install-manifest.json");
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        write_manifest(&manifest_path, &manifest, (uid, gid)).unwrap();
        let runner =
            FakeRunner::with_outputs(BINARY_NAMES.iter().map(|_| success(&commit)).collect());
        assert_eq!(
            read_and_verify_manifest_owned(&manifest_path, &runner, uid).unwrap(),
            manifest
        );
        assert_eq!(
            std::fs::metadata(&manifest_path).unwrap().mode() & 0o777,
            0o600
        );
        let daemon = render_daemon_unit(&source).unwrap();
        assert!(daemon.contains(&format!(
            "ExecStart={}/target/release/devcoordinator2 daemon",
            source.display()
        )));
        assert!(!daemon.contains("python"));
        let edge = render_edge_unit(&source, false).unwrap();
        assert!(edge.contains("ProtectHome=read-only"));
        assert!(edge.contains(&format!("ReadOnlyPaths={}", source.display())));
        let plan = installation_plan(&manifest, &manifest_path).unwrap();
        assert_eq!(
            plan.binary_links["/usr/local/bin/devcoordinator2"],
            source
                .join("target/release/devcoordinator2")
                .display()
                .to_string()
        );
        let destination = temporary.path().join("bin/devcoordinator2");
        replace_direct_link(&destination, &source.join("target/release/devcoordinator2")).unwrap();
        assert_eq!(
            std::fs::read_link(destination).unwrap(),
            source.join("target/release/devcoordinator2")
        );
        let changed = source.join("target/release/devcoordinator2");
        std::fs::write(&changed, "changed").unwrap();
        std::fs::set_permissions(&changed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner =
            FakeRunner::with_outputs(BINARY_NAMES.iter().map(|_| success(&commit)).collect());
        assert!(
            read_and_verify_manifest_owned(&manifest_path, &runner, uid)
                .unwrap_err()
                .contains("no longer match")
        );
    }

    #[test]
    fn manifest_detects_binary_tampering_and_links_refuse_regular_targets() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("tool");
        std::fs::write(&destination, "owned").unwrap();
        assert!(
            replace_direct_link(&destination, Path::new("/source"))
                .unwrap_err()
                .contains("non-symlink")
        );
    }

    #[test]
    fn environment_updates_are_atomic_idempotent_and_conflict_aware() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("instance.env");
        std::fs::write(&path, "A=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(ensure_env_value(&path, "B", "2").unwrap());
        assert!(!ensure_env_value(&path, "B", "2").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A=1\nB=2\n");
        assert!(
            ensure_env_value(&path, "B", "3")
                .unwrap_err()
                .contains("different installed value")
        );
        assert!(set_env_value(&path, "B", "3").unwrap());
        assert!(!set_env_value(&path, "B", "3").unwrap());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "A=1\nB=3\n");
    }

    #[test]
    fn private_policies_merge_without_dropping_existing_accounts() {
        let temporary = tempfile::tempdir().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let compose = temporary.path().join("compose.json");
        std::fs::write(
            &compose,
            serde_json::to_vec(&ComposePolicy {
                schema: 1,
                authorizations: vec![ComposeAuthorization {
                    repository_id: format!("r{}", "a".repeat(16)),
                    path: "a.env".to_owned(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            merge_compose_env_allowlist(
                &compose,
                &[ComposeAuthorization {
                    repository_id: format!("r{}", "b".repeat(16)),
                    path: "b.env".to_owned(),
                }],
                (uid, gid),
            )
            .unwrap()
        );
        let policy: ComposePolicy =
            serde_json::from_slice(&std::fs::read(&compose).unwrap()).unwrap();
        assert_eq!(policy.authorizations.len(), 2);
        assert_eq!(std::fs::metadata(&compose).unwrap().mode() & 0o777, 0o640);

        let usage = temporary.path().join("usage.json");
        let first = CodexUsageSource {
            uid: 1000,
            codex_home: "/home/one/.codex".to_owned(),
            executable: "/home/one/.local/bin/codex".to_owned(),
        };
        assert!(
            merge_codex_usage_sources(&usage, std::slice::from_ref(&first), (uid, gid)).unwrap()
        );
        assert!(
            merge_codex_usage_sources(
                &usage,
                &[CodexUsageSource {
                    uid: 1001,
                    codex_home: "/home/two/.codex".to_owned(),
                    executable: "/home/two/.local/bin/codex".to_owned(),
                }],
                (uid, gid),
            )
            .unwrap()
        );
        let policy: UsagePolicy = serde_json::from_slice(&std::fs::read(&usage).unwrap()).unwrap();
        assert_eq!(policy.sources.len(), 2);
        assert_eq!(policy.sources[0], first);
        assert_eq!(std::fs::metadata(&usage).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn compose_authorization_binds_one_ignored_regular_file_to_git_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repository.join(".gitignore"), "private/dev.env\n").unwrap();
        std::fs::create_dir(repository.join("private")).unwrap();
        std::fs::write(repository.join("private/dev.env"), "VALUE=not-read\n").unwrap();
        let entries = compose_env_authorizations(
            &[format!("{}=private/dev.env", repository.display())],
            &HostRunner,
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].repository_id.starts_with('r'));
        assert_eq!(entries[0].path, "private/dev.env");
        assert!(
            compose_env_authorizations(
                &[format!("{}=../outside.env", repository.display())],
                &HostRunner,
            )
            .unwrap_err()
            .contains("normalized relative")
        );
    }

    #[test]
    fn codex_source_resolution_uses_only_the_account_default_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(home.join(".local/bin/codex"), "binary").unwrap();
        let sources = codex_usage_sources(&["developer".to_owned()], |_| {
            Ok(LocalAccount {
                uid: 1234,
                gid: 1234,
                home: home.clone(),
            })
        })
        .unwrap();
        assert_eq!(
            sources,
            [CodexUsageSource {
                uid: 1234,
                codex_home: home.join(".codex").display().to_string(),
                executable: home.join(".local/bin/codex").display().to_string(),
            }]
        );
    }

    #[test]
    fn every_registered_schema_two_configuration_is_validated_by_the_built_binary() {
        let temporary = tempfile::tempdir().unwrap();
        let worktree = temporary.path().join("repo");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join(".devcoordinator.toml"), "schema = 2\n").unwrap();
        let database = temporary.path().join("authority.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(include_str!("../../control/src/schema.sql"))
            .unwrap();
        connection
            .execute(
                "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1',?1,'repo','t',1,'t')",
                [worktree.display().to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO worktrees VALUES('w1','r1',?1,'t','t')",
                [worktree.display().to_string()],
            )
            .unwrap();
        drop(connection);
        let binary = temporary.path().join("devcoordinator2");
        std::fs::write(&binary, "binary").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = FakeRunner::with_outputs(vec![success(
            "{\"schema\":2,\"tests\":[\"unit\",\"release\"],\"deployments\":[\"web\"]}\n",
        )]);
        let receipts = validate_registered_repository_configs(&database, &binary, &runner).unwrap();
        assert_eq!(
            receipts,
            [RepositoryConfigReceipt {
                worktree: worktree.display().to_string(),
                tests: vec!["unit".to_owned(), "release".to_owned()],
                deployments: vec!["web".to_owned()],
            }]
        );
        let request = &runner.requests.lock().unwrap()[0];
        assert_eq!(request.program, binary);
        assert_eq!(
            request.args,
            [
                OsString::from("--validate-repository-config"),
                worktree.as_os_str().to_owned()
            ]
        );
    }

    #[test]
    fn identity_setup_uses_exact_account_commands_and_preserves_numeric_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let account = LocalAccount {
            uid: 1000,
            gid: 1001,
            home: temporary.path().join("developer"),
        };
        let edge = LocalAccount {
            uid: 1100,
            gid: 1100,
            home: PathBuf::from("/var/lib/devcoordinator2-edge"),
        };
        let directory = StaticIdentities {
            gid: 1200,
            accounts: BTreeMap::from([
                ("developer".to_owned(), account),
                ("devcoordinator2-edge".to_owned(), edge.clone()),
            ]),
        };
        let runner = AlwaysRunner::default();
        let (receipt, selected_edge) = ensure_identities(
            "devcoordinator2-clients",
            &["developer".to_owned()],
            "devcoordinator2-edge",
            &runner,
            &directory,
        )
        .unwrap();
        assert_eq!(receipt.client_gid, 1200);
        assert_eq!(receipt.edge_uid, 1100);
        assert_eq!(selected_edge, edge);
        let requests = runner.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.program == Path::new("/usr/sbin/usermod"))
        );
        assert!(requests.iter().all(|request| request.clear_environment));
    }

    #[test]
    fn host_command_runner_drains_but_retains_only_bounded_output() {
        let output = HostRunner
            .run(&CommandRequest {
                program: "/usr/bin/seq".into(),
                args: vec!["1".into(), "50000".into()],
                environment: BTreeMap::new(),
                clear_environment: true,
            })
            .unwrap();
        assert!(output.success);
        assert!(output.stdout_truncated);
        assert!(output.stdout.len() <= OUTPUT_CAP);
        assert!(!output.stderr_truncated);
    }

    #[test]
    fn instance_preparation_is_private_idempotent_and_edge_read_only() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        for relative in ["edge/lib/module.mjs", "console/app.js"] {
            let path = source.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "source").unwrap();
        }
        std::fs::create_dir_all(source.join("deploy")).unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.tmpfiles.conf"),
            "d /run/devcoordinator2 0755 root root -\n",
        )
        .unwrap();
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let paths = PreparePaths {
            etc_dir: temporary.path().join("etc"),
            runtime_dir: temporary.path().join("run"),
            state_dir: temporary.path().join("state"),
            bugs_dir: temporary.path().join("bugs"),
            edge_state_dir: temporary.path().join("edge-state"),
            tmpfiles_target: temporary.path().join("tmpfiles/devcoordinator2.conf"),
        };
        let request = PrepareRequest {
            source_root: source.clone(),
            base_domain: "example.test".to_owned(),
            admin_emails: "owner@example.test".to_owned(),
            identities: IdentityReceipt {
                client_group: "devcoordinator2-clients".to_owned(),
                client_gid: gid,
                edge_uid: uid,
                edge_gid: gid,
                client_accounts: vec!["developer".to_owned()],
            },
            edge_account: LocalAccount {
                uid,
                gid,
                home: paths.edge_state_dir.clone(),
            },
            system_owner: (uid, gid),
            canary: true,
            canary_port: 28080,
            compose_authorizations: vec![ComposeAuthorization {
                repository_id: format!("r{}", "a".repeat(16)),
                path: "private/dev.env".to_owned(),
            }],
            usage_sources: vec![CodexUsageSource {
                uid,
                codex_home: "/home/developer/.codex".to_owned(),
                executable: "/home/developer/.local/bin/codex".to_owned(),
            }],
            paths: paths.clone(),
        };
        let runner = AlwaysRunner::default();
        let first = prepare_instance(&request, &runner).unwrap();
        assert!(first.created_instance_env);
        assert!(first.created_edge_env);
        assert!(first.created_session_secret);
        assert!(first.compose_policy_changed);
        assert!(first.usage_policy_changed);
        let second = prepare_instance(&request, &runner).unwrap();
        assert!(!second.created_instance_env);
        assert!(!second.created_edge_env);
        assert!(!second.created_session_secret);
        assert!(!second.compose_policy_changed);
        assert!(!second.usage_policy_changed);
        assert_eq!(
            std::fs::metadata(paths.etc_dir.join("codex-usage-sources.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            std::fs::read_to_string(paths.etc_dir.join("edge.env"))
                .unwrap()
                .contains(&format!("EDGE_CONSOLE_DIR={}/console", source.display()))
        );
        let requests = runner.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.program == Path::new("/usr/bin/setfacl"))
        );
        assert!(requests.iter().all(|request| {
            !request
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().contains("rwx"))
        }));
    }

    #[test]
    fn state_repair_preserves_contents_and_public_paths_before_restoring_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let owner = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        for name in ["public", "deployments", "secrets", "cutover", "recovery"] {
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::fs::write(root.join(name).join("keep"), "unchanged").unwrap();
        }
        for name in [
            "authority.sqlite3",
            "authority.sqlite3-wal",
            "authority.sqlite3-shm",
            "authority.sqlite3.pre-schema7.bak",
        ] {
            std::fs::write(root.join(name), "preserved").unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        for _ in 0..2 {
            // Simulate the former startup rule, then repair the same state again.
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o750)).unwrap();
            prepare_state_directory(root, owner).unwrap();
            assert_eq!(std::fs::metadata(root).unwrap().mode() & 0o777, 0o751);
            for name in [
                "authority.sqlite3",
                "authority.sqlite3-wal",
                "authority.sqlite3-shm",
                "authority.sqlite3.pre-schema7.bak",
            ] {
                let metadata = std::fs::metadata(root.join(name)).unwrap();
                assert_eq!(metadata.mode() & 0o777, 0o600);
                assert_eq!((metadata.uid(), metadata.gid()), owner);
                assert_eq!(
                    std::fs::read_to_string(root.join(name)).unwrap(),
                    "preserved"
                );
            }
            for name in ["public", "deployments", "secrets", "cutover", "recovery"] {
                assert_eq!(
                    std::fs::metadata(root.join(name)).unwrap().mode() & 0o777,
                    if matches!(name, "cutover" | "recovery") {
                        0o700
                    } else {
                        0o755
                    }
                );
                assert_eq!(
                    std::fs::read_to_string(root.join(name).join("keep")).unwrap(),
                    "unchanged"
                );
            }
        }
    }

    #[test]
    fn state_repair_refuses_unsafe_entries_before_changing_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        let authority = root.join("authority.sqlite3");
        std::fs::write(&authority, "preserved").unwrap();
        std::fs::set_permissions(&authority, std::fs::Permissions::from_mode(0o644)).unwrap();
        let owner = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        symlink(&authority, root.join("authority.sqlite3-wal")).unwrap();
        assert!(prepare_state_directory(&root, owner).is_err());
        assert_eq!(std::fs::metadata(&authority).unwrap().mode() & 0o777, 0o644);
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o750);
        std::fs::remove_file(root.join("authority.sqlite3-wal")).unwrap();
        std::fs::write(root.join("unclassified"), "preserved").unwrap();
        assert!(prepare_state_directory(&root, owner).is_err());
        assert!(prepare_state_directory(&root, (owner.0 + 1, owner.1)).is_err());
        assert_eq!(std::fs::metadata(&authority).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn state_tmpfiles_retains_instance_rules_and_the_installer_traversal_mode() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("deploy")).unwrap();
        std::fs::write(
            temp.path().join("deploy/devcoordinator2.tmpfiles.conf"),
            include_str!("../../../deploy/devcoordinator2.tmpfiles.conf"),
        )
        .unwrap();
        let installed = temp.path().join("installed.conf");
        let other = "d /var/lib/devcoordinator2-bugs 2775 root devcoordinator2-clients -\n";
        std::fs::write(
            &installed,
            format!("d /var/lib/devcoordinator2 0750 root root -\n{other}"),
        )
        .unwrap();
        let rendered = state_tmpfiles(temp.path(), &installed).unwrap();
        assert_eq!(
            rendered,
            format!("d /var/lib/devcoordinator2 0751 root root -\n{other}")
        );
        std::fs::write(&installed, &rendered).unwrap();
        assert_eq!(state_tmpfiles(temp.path(), &installed).unwrap(), rendered);
    }
}
