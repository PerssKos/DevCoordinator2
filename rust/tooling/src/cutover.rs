//! Fail-closed activation state machine for the one live Rust cutover.
//!
//! Concrete host access is an adapter so the sequence can be exhaustively
//! rehearsed. The database backup is restored when integrity fails or a failed
//! candidate changed the schema beyond the captured prior installation. A
//! same-schema startup or acceptance failure keeps intact data and restores
//! only units, links, and the socket.

use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use devcoordinator2_api::DATABASE_SCHEMA_VERSION;
use rusqlite::{Connection, OpenFlags};
use rustix::fs::{self as unix_fs, FlockOperation, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::install::{
    self, CommandRequest, CommandRunner, InstallManifest, read_and_verify_manifest_owned,
    render_daemon_unit, render_edge_unit,
};

const DRAIN_FILE: &str = "test-drain.json";
const ACTIVITY_FILE: &str = "test-activity.json";
const LOCK_FILE: &str = "test-admission.lock";
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;
const MINIMUM_SUPPORTED_DATABASE_SCHEMA: u32 = 15;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutoverReceipt {
    pub status: String,
    pub backup: String,
    pub database_restored: bool,
    pub checks: Vec<String>,
}

pub trait CutoverAdapter {
    type Drain;
    type InstallationSnapshot;

    fn close_admission(&mut self) -> Result<Self::Drain, String>;
    fn wait_for_quiescence(&mut self, drain: &Self::Drain) -> Result<(), String>;
    fn wait_for_connections(&mut self) -> Result<(), String>;
    fn deployment_is_applying(&mut self) -> Result<bool, String>;
    fn backup_database(&mut self) -> Result<String, String>;
    fn capture_installation(&mut self) -> Result<Self::InstallationSnapshot, String>;
    fn fence_legacy_socket(&mut self) -> Result<(), String>;
    fn stop_legacy(&mut self) -> Result<(), String>;
    fn install_rust(&mut self) -> Result<(), String>;
    fn start_rust(&mut self) -> Result<(), String>;
    fn verify_live(&mut self) -> Result<Vec<String>, String>;
    fn commit_activation(&mut self, snapshot: &Self::InstallationSnapshot) -> Result<(), String>;
    fn stop_rust(&mut self) -> Result<(), String>;
    fn restore_installation(&mut self, snapshot: &Self::InstallationSnapshot)
    -> Result<(), String>;
    fn database_matches_backup(&mut self, backup: &str) -> Result<bool, String>;
    fn restore_database(&mut self, backup: &str) -> Result<(), String>;
    fn restore_legacy_socket(&mut self) -> Result<(), String>;
    fn finish_rollback(&mut self, _snapshot: &Self::InstallationSnapshot) -> Result<(), String> {
        Ok(())
    }
    fn start_legacy(&mut self) -> Result<(), String>;
    fn reopen_admission(&mut self, drain: Self::Drain) -> Result<(), String>;
}

pub fn activate<A: CutoverAdapter>(adapter: &mut A) -> Result<CutoverReceipt, String> {
    let drain = adapter.close_admission()?;
    let outcome = activate_drained(adapter, &drain);
    let reopen = adapter.reopen_admission(drain);
    match (outcome, reopen) {
        (Ok(receipt), Ok(())) => Ok(receipt),
        (Ok(_), Err(error)) => Err(format!(
            "Rust activated but test admission could not reopen: {error}"
        )),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(reopen)) => Err(format!(
            "{error}; test admission also could not reopen: {reopen}"
        )),
    }
}

fn activate_drained<A: CutoverAdapter>(
    adapter: &mut A,
    drain: &A::Drain,
) -> Result<CutoverReceipt, String> {
    adapter.wait_for_quiescence(drain)?;
    adapter.fence_legacy_socket()?;
    if let Err(error) = adapter.wait_for_connections() {
        return Err(combine(
            error,
            "restore legacy socket",
            adapter.restore_legacy_socket(),
        ));
    }
    match adapter.deployment_is_applying() {
        Ok(false) => {}
        Ok(true) => {
            return Err(combine(
                "cutover blocked while a deployment is applying".to_owned(),
                "restore legacy socket",
                adapter.restore_legacy_socket(),
            ));
        }
        Err(error) => {
            return Err(combine(
                error,
                "restore legacy socket",
                adapter.restore_legacy_socket(),
            ));
        }
    }
    let backup = match adapter.backup_database() {
        Ok(backup) => backup,
        Err(error) => {
            return Err(combine(
                error,
                "restore legacy socket",
                adapter.restore_legacy_socket(),
            ));
        }
    };
    let snapshot = match adapter.capture_installation() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Err(combine(
                error,
                "restore legacy socket",
                adapter.restore_legacy_socket(),
            ));
        }
    };
    if let Err(error) = adapter.stop_legacy() {
        let error = combine(
            error,
            "restore legacy socket",
            adapter.restore_legacy_socket(),
        );
        return Err(combine(
            error,
            "restart prior services",
            adapter.start_legacy(),
        ));
    }
    let activation = (|| {
        adapter.install_rust()?;
        adapter.start_rust()?;
        let checks = adapter.verify_live()?;
        adapter.commit_activation(&snapshot)?;
        Ok(checks)
    })();
    match activation {
        Ok(checks) => Ok(CutoverReceipt {
            status: "activated".to_owned(),
            backup,
            database_restored: false,
            checks,
        }),
        Err(error) => rollback(adapter, &snapshot, &backup, error),
    }
}

fn rollback<A: CutoverAdapter>(
    adapter: &mut A,
    snapshot: &A::InstallationSnapshot,
    backup: &str,
    activation_error: String,
) -> Result<CutoverReceipt, String> {
    let mut failures = Vec::new();
    if let Err(error) = adapter.stop_rust() {
        failures.push(format!("stop Rust: {error}"));
    }
    if let Err(error) = adapter.restore_installation(snapshot) {
        failures.push(format!("restore installation: {error}"));
    }
    let mut database_restored = false;
    match adapter.database_matches_backup(backup) {
        Ok(true) => {}
        Ok(false) => match adapter.restore_database(backup) {
            Ok(()) => database_restored = true,
            Err(error) => failures.push(format!("restore database: {error}")),
        },
        Err(error) => failures.push(format!("check database rollback compatibility: {error}")),
    }
    if let Err(error) = adapter.restore_legacy_socket() {
        failures.push(format!("restore legacy socket: {error}"));
    }
    if let Err(error) = adapter.start_legacy() {
        failures.push(format!("restart prior service: {error}"));
    }
    if failures.is_empty()
        && let Err(error) = adapter.finish_rollback(snapshot)
    {
        failures.push(error);
    }
    let suffix = if failures.is_empty() {
        format!(
            "rollback restored the prior service{}",
            if database_restored {
                " and database backup"
            } else {
                " without replacing the intact database"
            }
        )
    } else {
        format!("rollback incomplete: {}", failures.join("; "))
    };
    Err(format!(
        "Rust activation failed: {activation_error}; {suffix}"
    ))
}

fn combine(primary: String, operation: &str, secondary: Result<(), String>) -> String {
    match secondary {
        Ok(()) => primary,
        Err(error) => format!("{primary}; {operation} failed: {error}"),
    }
}

#[derive(Clone, Debug)]
pub struct HostCutoverConfig {
    pub candidate_manifest: PathBuf,
    pub installed_manifest: PathBuf,
    pub tmpfiles_path: PathBuf,
    pub transaction_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub database_path: PathBuf,
    pub daemon_unit_path: PathBuf,
    pub edge_unit_path: PathBuf,
    pub cli_link: PathBuf,
    pub tooling_link: PathBuf,
    pub systemctl: PathBuf,
    pub ss: PathBuf,
    pub instance_env: PathBuf,
    pub daemon_unit: String,
    pub edge_unit: String,
    pub edge_state_dir: PathBuf,
    pub canary: bool,
}

#[derive(Clone, Debug)]
pub struct RecoveryConfig {
    pub transaction_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub database_path: PathBuf,
    pub systemctl: PathBuf,
    pub daemon_unit: String,
    pub edge_unit: String,
    pub edge_state_dir: PathBuf,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            transaction_dir: PathBuf::new(),
            runtime_dir: "/run/devcoordinator2".into(),
            socket_path: "/run/devcoordinator2/daemon.sock".into(),
            database_path: "/var/lib/devcoordinator2/authority.sqlite3".into(),
            systemctl: "/usr/bin/systemctl".into(),
            daemon_unit: "devcoordinator2.service".to_owned(),
            edge_unit: "devcoordinator2-edge.service".to_owned(),
            edge_state_dir: "/var/lib/devcoordinator2-edge".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReceipt {
    pub status: String,
    pub database_restored: bool,
    pub snapshot: String,
}

impl Default for HostCutoverConfig {
    fn default() -> Self {
        Self {
            candidate_manifest: "/etc/devcoordinator2/candidate-install-manifest.json".into(),
            installed_manifest: "/etc/devcoordinator2/install-manifest.json".into(),
            tmpfiles_path: "/etc/tmpfiles.d/devcoordinator2.conf".into(),
            transaction_dir: "/var/lib/devcoordinator2/cutover/rust-v2".into(),
            runtime_dir: "/run/devcoordinator2".into(),
            socket_path: "/run/devcoordinator2/daemon.sock".into(),
            database_path: "/var/lib/devcoordinator2/authority.sqlite3".into(),
            daemon_unit_path: "/etc/systemd/system/devcoordinator2.service".into(),
            edge_unit_path: "/etc/systemd/system/devcoordinator2-edge.service".into(),
            cli_link: "/usr/local/bin/devcoordinator2".into(),
            tooling_link: "/usr/local/bin/devcoordinator2-tooling".into(),
            systemctl: "/usr/bin/systemctl".into(),
            ss: "/usr/bin/ss".into(),
            instance_env: "/etc/devcoordinator2/instance.env".into(),
            daemon_unit: "devcoordinator2.service".to_owned(),
            edge_unit: "devcoordinator2-edge.service".to_owned(),
            edge_state_dir: "/var/lib/devcoordinator2-edge".into(),
            canary: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSnapshot {
    pub schema: u8,
    pub status: String,
    pub transaction_dir: String,
    pub entries: Vec<SnapshotEntry>,
    pub backup_sha256: String,
    pub candidate_commit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    pub path: String,
    pub kind: SnapshotKind,
    pub content_base64: Option<String>,
    pub link_target: Option<String>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Missing,
    File,
    Symlink,
}

#[derive(Clone, Debug)]
pub struct HostDrain {
    runtime_dir: PathBuf,
    nonce: String,
}

pub struct HostCutover {
    config: HostCutoverConfig,
    runner: Arc<dyn CommandRunner>,
    manifest: InstallManifest,
    daemon_unit_text: String,
    edge_unit_text: String,
    tmpfiles_text: String,
    expected_owner: (u32, u32),
    fenced: bool,
}

impl HostCutover {
    pub fn new(config: HostCutoverConfig, runner: Arc<dyn CommandRunner>) -> Result<Self, String> {
        Self::new_owned(config, runner, (0, 0))
    }

    pub fn new_owned(
        config: HostCutoverConfig,
        runner: Arc<dyn CommandRunner>,
        expected_owner: (u32, u32),
    ) -> Result<Self, String> {
        match config.transaction_dir.symlink_metadata() {
            Ok(_) => return Err("cutover transaction directory already exists".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect cutover transaction: {error}")),
        }
        let manifest = read_and_verify_manifest_owned(
            &config.candidate_manifest,
            runner.as_ref(),
            expected_owner.0,
        )?;
        let source_root = Path::new(&manifest.source_root);
        let daemon_unit_text = render_daemon_unit(source_root)?;
        let edge_unit_text = render_edge_unit(source_root, config.canary)?;
        let tmpfiles_text = install::state_tmpfiles(source_root, &config.tmpfiles_path)?;
        Ok(Self {
            config,
            runner,
            manifest,
            daemon_unit_text,
            edge_unit_text,
            tmpfiles_text,
            expected_owner,
            fenced: false,
        })
    }

    fn systemctl(&self, arguments: &[&str]) -> Result<(), String> {
        let output = self.runner.run(&CommandRequest {
            program: self.config.systemctl.clone(),
            args: arguments.iter().map(OsString::from).collect(),
            environment: std::collections::BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            clear_environment: true,
        })?;
        if output.success {
            Ok(())
        } else {
            Err(command_failure("systemctl", &output.stderr, &output.stdout))
        }
    }

    fn systemctl_all(&self, actions: &[(&str, &str)]) -> Result<(), String> {
        let mut failures = Vec::new();
        for (action, unit) in actions {
            if let Err(error) = self.systemctl(&[action, unit]) {
                failures.push(format!("{action} {unit}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn snapshot_targets(&self) -> [&Path; 6] {
        [
            &self.config.daemon_unit_path,
            &self.config.edge_unit_path,
            &self.config.cli_link,
            &self.config.tooling_link,
            &self.config.installed_manifest,
            &self.config.tmpfiles_path,
        ]
    }

    fn installed_binary(&self, name: &str) -> Result<PathBuf, String> {
        self.manifest
            .binaries
            .iter()
            .find(|binary| binary.name == name)
            .map(|binary| PathBuf::from(&binary.path))
            .ok_or_else(|| format!("candidate manifest has no {name} binary"))
    }
}

impl CutoverAdapter for HostCutover {
    type Drain = HostDrain;
    type InstallationSnapshot = InstallationSnapshot;

    fn close_admission(&mut self) -> Result<Self::Drain, String> {
        begin_drain(&self.config.runtime_dir, self.expected_owner)
    }

    fn wait_for_quiescence(&mut self, _drain: &Self::Drain) -> Result<(), String> {
        wait_for_zero_activity(&self.config.runtime_dir)
    }

    fn wait_for_connections(&mut self) -> Result<(), String> {
        wait_for_socket_connections(
            self.runner.as_ref(),
            &self.config.ss,
            &self.config.socket_path,
        )?;
        let directory = crate::instance::parse_env(&self.config.instance_env)
            .get("DEVCOORDINATOR2_SANDBOX_BRIDGE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if self.config.socket_path == Path::new("/run/devcoordinator2/daemon.sock") {
                    PathBuf::from("/tmp/devcoordinator2-bridge")
                } else {
                    self.config.socket_path.with_file_name("sandbox-bridge")
                }
            });
        wait_for_bridge_connections(&directory)
    }

    fn deployment_is_applying(&mut self) -> Result<bool, String> {
        let connection = open_database_read_only(&self.config.database_path)?;
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM deployments WHERE state='applying')",
                [],
                |row| Ok(row.get::<_, i64>(0)? != 0),
            )
            .map_err(|error| format!("cannot inspect deployment activity: {error}"))
    }

    fn backup_database(&mut self) -> Result<String, String> {
        install::ensure_owned_directory(&self.config.transaction_dir, 0o700, self.expected_owner)
            .map_err(|error| format!("cannot prepare cutover transaction: {error}"))?;
        let backup = self.config.transaction_dir.join("authority-before.sqlite3");
        if backup.exists() {
            return Err("cutover database backup already exists".to_owned());
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup)
            .map_err(|error| format!("cannot create private database backup: {error}"))?;
        let connection = open_database_read_only(&self.config.database_path)?;
        connection
            .backup(rusqlite::MAIN_DB, &backup, None)
            .map_err(|error| format!("cannot back up Coordinator database: {error}"))?;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot protect database backup: {error}"))?;
        if !database_integrity(&backup)? {
            return Err("captured database backup failed integrity verification".into());
        }
        path_text(&backup)
    }

    fn capture_installation(&mut self) -> Result<Self::InstallationSnapshot, String> {
        let entries = self
            .snapshot_targets()
            .into_iter()
            .map(capture_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let snapshot = InstallationSnapshot {
            schema: 2,
            status: "prepared".to_owned(),
            transaction_dir: path_text(&self.config.transaction_dir)?,
            entries,
            backup_sha256: install::hash_file(
                &self.config.transaction_dir.join("authority-before.sqlite3"),
            )?,
            candidate_commit: self.manifest.source_commit.clone(),
        };
        write_snapshot(&snapshot, "prepared", self.expected_owner)?;
        Ok(snapshot)
    }

    fn fence_legacy_socket(&mut self) -> Result<(), String> {
        let fence = self.config.runtime_dir.join("daemon.pre-cutover.sock");
        match fence.symlink_metadata() {
            Ok(_) => return Err("stale cutover socket fence exists".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect cutover socket fence: {error}")),
        }
        let metadata = self
            .config
            .socket_path
            .symlink_metadata()
            .map_err(|error| format!("cannot inspect live daemon socket: {error}"))?;
        if metadata.file_type().is_symlink()
            || (!metadata.file_type().is_socket() && !metadata.is_file())
        {
            return Err("live daemon socket has an unsafe file type".to_owned());
        }
        std::fs::rename(&self.config.socket_path, &fence)
            .map_err(|error| format!("cannot fence live daemon socket: {error}"))?;
        self.fenced = true;
        Ok(())
    }

    fn stop_legacy(&mut self) -> Result<(), String> {
        self.systemctl_all(&[
            ("stop", &self.config.edge_unit),
            ("stop", &self.config.daemon_unit),
        ])
    }

    fn install_rust(&mut self) -> Result<(), String> {
        install::prepare_state_directory(
            self.config
                .database_path
                .parent()
                .ok_or("database has no state directory")?,
            self.expected_owner,
        )?;
        install::atomic_file(
            &self.config.tmpfiles_path,
            self.tmpfiles_text.as_bytes(),
            0o644,
            Some(self.expected_owner),
        )?;
        install::atomic_file(
            &self.config.daemon_unit_path,
            self.daemon_unit_text.as_bytes(),
            0o644,
            Some(self.expected_owner),
        )?;
        install::atomic_file(
            &self.config.edge_unit_path,
            self.edge_unit_text.as_bytes(),
            0o644,
            Some(self.expected_owner),
        )?;
        install::write_manifest(
            &self.config.installed_manifest,
            &self.manifest,
            self.expected_owner,
        )?;
        replace_captured_target(
            &self.config.cli_link,
            &self.installed_binary("devcoordinator2")?,
        )?;
        replace_captured_target(
            &self.config.tooling_link,
            &self.installed_binary("devcoordinator2-tooling")?,
        )?;
        self.systemctl(&["daemon-reload"])
    }

    fn start_rust(&mut self) -> Result<(), String> {
        self.systemctl_all(&[
            ("enable", &self.config.daemon_unit),
            ("restart", &self.config.daemon_unit),
            ("enable", &self.config.edge_unit),
            ("restart", &self.config.edge_unit),
        ])
    }

    fn verify_live(&mut self) -> Result<Vec<String>, String> {
        let binary = self.installed_binary("devcoordinator2")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let output = self.runner.run(&CommandRequest {
                program: binary.clone(),
                args: vec!["ping".into()],
                environment: std::collections::BTreeMap::from([
                    (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                    (
                        OsString::from("DEVCOORDINATOR2_INSTANCE_ENV"),
                        self.config.instance_env.as_os_str().to_owned(),
                    ),
                ]),
                clear_environment: true,
            })?;
            let response = serde_json::from_str::<Value>(&output.stdout).ok();
            if output.success && response.as_ref().is_some_and(successful_v2_ping) {
                break;
            }
            if !response.as_ref().is_some_and(retryable_v2_ping)
                || std::time::Instant::now() >= deadline
            {
                return Err(command_failure("v2 ping", &output.stderr, &output.stdout));
            }
            self.systemctl(&["is-active", "--quiet", &self.config.daemon_unit])?;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.systemctl(&["is-active", "--quiet", &self.config.daemon_unit])?;
        self.systemctl(&["is-active", "--quiet", &self.config.edge_unit])?;
        verify_reconciled_routes(&self.config.database_path, &self.config.edge_state_dir)?;
        Ok(vec![
            "route-lease-edge-reconciled".to_owned(),
            "v2-cli-ping".to_owned(),
            "rust-daemon-active".to_owned(),
            "node-edge-active".to_owned(),
        ])
    }

    fn commit_activation(&mut self, snapshot: &Self::InstallationSnapshot) -> Result<(), String> {
        write_snapshot(snapshot, "committed", self.expected_owner)?;
        let fence = self.config.runtime_dir.join("daemon.pre-cutover.sock");
        if fence.exists() {
            std::fs::remove_file(&fence)
                .map_err(|error| format!("cannot retire legacy socket fence: {error}"))?;
        }
        self.fenced = false;
        Ok(())
    }

    fn stop_rust(&mut self) -> Result<(), String> {
        self.systemctl_all(&[
            ("stop", &self.config.edge_unit),
            ("stop", &self.config.daemon_unit),
        ])
    }

    fn restore_installation(
        &mut self,
        snapshot: &Self::InstallationSnapshot,
    ) -> Result<(), String> {
        for entry in snapshot.entries.iter().rev() {
            restore_entry(entry)?;
        }
        self.systemctl(&["daemon-reload"])?;
        write_snapshot(snapshot, "rolling_back", self.expected_owner)
    }

    fn finish_rollback(&mut self, snapshot: &Self::InstallationSnapshot) -> Result<(), String> {
        write_snapshot(snapshot, "rolled_back", self.expected_owner)
    }

    fn database_matches_backup(&mut self, backup: &str) -> Result<bool, String> {
        if !database_integrity(&self.config.database_path)? {
            return Ok(false);
        }
        if !database_integrity(Path::new(backup))? {
            return Err("cutover database backup failed integrity verification".to_owned());
        }
        Ok(database_schema(&self.config.database_path)? == database_schema(Path::new(backup))?)
    }

    fn restore_database(&mut self, backup: &str) -> Result<(), String> {
        if !database_integrity(Path::new(backup))? {
            return Err("cutover database backup failed integrity verification".to_owned());
        }
        for suffix in ["-wal", "-shm"] {
            remove_file_if_present(Path::new(&format!(
                "{}{suffix}",
                self.config.database_path.display()
            )))?;
        }
        atomic_copy(
            Path::new(backup),
            &self.config.database_path,
            self.expected_owner,
        )
    }

    fn restore_legacy_socket(&mut self) -> Result<(), String> {
        let fence = self.config.runtime_dir.join("daemon.pre-cutover.sock");
        if !fence.exists() {
            self.fenced = false;
            return Ok(());
        }
        remove_file_if_present(&self.config.socket_path)?;
        std::fs::rename(&fence, &self.config.socket_path)
            .map_err(|error| format!("cannot restore legacy daemon socket: {error}"))?;
        self.fenced = false;
        Ok(())
    }

    fn start_legacy(&mut self) -> Result<(), String> {
        self.systemctl_all(&[
            ("restart", &self.config.daemon_unit),
            ("restart", &self.config.edge_unit),
        ])
    }

    fn reopen_admission(&mut self, drain: Self::Drain) -> Result<(), String> {
        end_drain(&drain)
    }
}

fn successful_v2_ping(response: &Value) -> bool {
    response.get("protocol").and_then(Value::as_u64) == Some(2)
        && response.get("ok") == Some(&Value::Bool(true))
}

fn retryable_v2_ping(response: &Value) -> bool {
    response.get("protocol").and_then(Value::as_u64) == Some(2)
        && response.get("ok") == Some(&Value::Bool(false))
        && response
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            == Some("daemon_unavailable")
}

pub fn recover_host(
    config: &RecoveryConfig,
    runner: &dyn CommandRunner,
    expected_owner: (u32, u32),
) -> Result<RecoveryReceipt, String> {
    if config.transaction_dir.as_os_str().is_empty() {
        return Err(
            "recovery target is required; specify one in-progress transaction directory".to_owned(),
        );
    }
    let invalid_target = |error| format!("recovery target invalid: {error}");
    let snapshot =
        read_snapshot(&config.transaction_dir, expected_owner.0).map_err(invalid_target)?;
    if !matches!(
        snapshot.status.as_str(),
        "prepared" | "activating" | "rolling_back"
    ) {
        return Err(format!(
            "recovery target is not an in-progress transaction (status {})",
            snapshot.status
        ));
    }
    // A prepared transaction from a different installation cannot be used as
    // a convenient old restore point. It must still match either the captured
    // prior installation or the candidate involved in this cutover.
    if let Some(entry) = snapshot
        .entries
        .iter()
        .find(|entry| entry.path.ends_with("/install-manifest.json"))
    {
        let current = read_json_file(Path::new(&entry.path)).ok().flatten();
        let prior_matches = std::fs::read(&entry.path).ok().is_some_and(|bytes| {
            entry
                .content_base64
                .as_ref()
                .is_some_and(|prior| BASE64.decode(prior).ok().as_deref() == Some(bytes.as_slice()))
        });
        if !prior_matches
            && !current.as_ref().is_some_and(|doc| {
                doc["source_commit"].as_str() == Some(snapshot.candidate_commit.as_str())
            })
        {
            return Err("recovery target belongs to a different installed candidate".into());
        }
    }
    let backup_path = config.transaction_dir.join("authority-before.sqlite3");
    if install::hash_file(&backup_path).map_err(invalid_target)? != snapshot.backup_sha256 {
        return Err("recovery target backup does not match its transaction".into());
    }
    if !database_integrity(&backup_path).map_err(invalid_target)? {
        return Err("recovery target backup failed integrity verification".into());
    }
    run_systemctl_all(runner, &config.systemctl, &[("stop", &config.daemon_unit)])?;
    for entry in snapshot.entries.iter().rev() {
        restore_entry(entry)?;
    }
    run_systemctl(runner, &config.systemctl, &["daemon-reload"])?;
    let backup = config.transaction_dir.join("authority-before.sqlite3");
    let database_restored = match (
        database_integrity(&config.database_path)?,
        database_integrity(&backup)?,
    ) {
        (true, true) if database_schema(&config.database_path)? == database_schema(&backup)? => {
            false
        }
        (_, true) => {
            for suffix in ["-wal", "-shm"] {
                remove_file_if_present(Path::new(&format!(
                    "{}{suffix}",
                    config.database_path.display()
                )))?;
            }
            atomic_copy(&backup, &config.database_path, expected_owner)?;
            true
        }
        (_, false) => {
            return Err("cutover database backup failed integrity verification".to_owned());
        }
    };
    restore_socket(&config.runtime_dir, &config.socket_path)?;
    clear_stale_drain(&config.runtime_dir)?;
    run_systemctl_all(
        runner,
        &config.systemctl,
        &[
            ("restart", &config.daemon_unit),
            ("restart", &config.edge_unit),
        ],
    )?;
    verify_reconciled_routes(&config.database_path, &config.edge_state_dir)?;
    write_snapshot(&snapshot, "recovered", expected_owner)?;
    Ok(RecoveryReceipt {
        status: "recovered".to_owned(),
        database_restored,
        snapshot: path_text(&config.transaction_dir.join("installation-snapshot.json"))?,
    })
}

/// Verify the same published snapshot the edge is actually serving. A service
/// being active is insufficient after database recovery.
pub fn verify_reconciled_routes(database_path: &Path, edge_state: &Path) -> Result<(), String> {
    let route_path = database_path
        .parent()
        .ok_or("route state directory is missing")?
        .join("public/routes.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut delay = std::time::Duration::from_millis(25);
    loop {
        let published = read_json_file(&route_path)?;
        let accepted = read_json_file(&edge_state.join("routes.accepted.json"))?;
        if let (Some(doc), Some(ack)) = (published, accepted)
            && doc["schema"] == 2
            && ack["schema"] == 2
            && doc["generation"].as_u64().is_some()
            && doc["payload_sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64)
            && doc["generation"] == ack["generation"]
            && doc["payload_sha256"] == ack["payload_sha256"]
        {
            let connection = open_database_read_only(database_path)?;
            let rows = doc["routes"]
                .as_array()
                .ok_or("route document has no routes")?;
            for route in rows {
                let port = route["port"]
                    .as_u64()
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or("route port is invalid")?;
                if route["observed"] == true {
                    // Imported applications keep their declared endpoint, but
                    // their lifecycle is outside this daemon's lease ownership.
                    // An external outage must not roll back a healthy Coordinator.
                    let valid: bool = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM observed_routes WHERE domain=?1 AND observed_deployment_id=?2 AND component=?3 AND port=?4)",
                        rusqlite::params![route["label"].as_str(), route["deployment_id"].as_str(), route["component"].as_str(), port],
                        |row|row.get(0),
                    ).map_err(|_| "cannot verify observed route reservation")?;
                    if !valid {
                        return Err(
                            "recovery incomplete: observed route reservation mismatch".into()
                        );
                    }
                } else {
                    let valid: bool = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM domain_routes r JOIN port_assignments p ON p.lease_id=r.lease_id AND p.port=r.port AND p.deployment_id=r.deployment_id AND p.component=r.component JOIN deployments d ON d.deployment_id=r.deployment_id WHERE p.generation=0 AND r.deployment_id=?1 AND r.component=?2 AND r.port=?3 AND r.lease_id=?4 AND r.domain=?5 AND r.generation=d.current_generation)",
                        rusqlite::params![route["deployment_id"].as_str(), route["component"].as_str(), port, route["lease_id"].as_str(), route["label"].as_str()],
                        |row|row.get(0),
                    ).map_err(|_| "cannot verify route leases")?;
                    if !valid {
                        return Err("recovery incomplete: route lease ownership mismatch".into());
                    }
                    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                    if std::net::TcpStream::connect_timeout(
                        &address,
                        std::time::Duration::from_secs(1),
                    )
                    .is_err()
                    {
                        return Err("recovery incomplete: routed listener is unavailable".into());
                    }
                }
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                "recovery incomplete: edge has not accepted the reconciled route document".into(),
            );
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(500));
    }
}

fn read_snapshot(
    transaction_dir: &Path,
    expected_uid: u32,
) -> Result<InstallationSnapshot, String> {
    let path = transaction_dir.join("installation-snapshot.json");
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("cannot inspect installation snapshot: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_STATE_BYTES
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != expected_uid
    {
        return Err("installation snapshot must be a private owned regular file".to_owned());
    }
    let value =
        read_json_file(&path)?.ok_or_else(|| "installation snapshot is unavailable".to_owned())?;
    let snapshot: InstallationSnapshot = serde_json::from_value(value)
        .map_err(|error| format!("installation snapshot is invalid: {error}"))?;
    if snapshot.schema != 2 || Path::new(&snapshot.transaction_dir) != transaction_dir {
        return Err("installation snapshot identity does not match its transaction".to_owned());
    }
    Ok(snapshot)
}

fn run_systemctl(
    runner: &dyn CommandRunner,
    systemctl: &Path,
    arguments: &[&str],
) -> Result<(), String> {
    let output = runner.run(&CommandRequest {
        program: systemctl.to_owned(),
        args: arguments.iter().map(OsString::from).collect(),
        environment: std::collections::BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/bin:/bin"),
        )]),
        clear_environment: true,
    })?;
    if output.success {
        Ok(())
    } else {
        Err(command_failure("systemctl", &output.stderr, &output.stdout))
    }
}

fn run_systemctl_all(
    runner: &dyn CommandRunner,
    systemctl: &Path,
    actions: &[(&str, &str)],
) -> Result<(), String> {
    let mut failures = Vec::new();
    for (action, unit) in actions {
        if let Err(error) = run_systemctl(runner, systemctl, &[action, unit]) {
            failures.push(format!("{action} {unit}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn restore_socket(runtime_dir: &Path, socket_path: &Path) -> Result<(), String> {
    let fence = runtime_dir.join("daemon.pre-cutover.sock");
    if fence.exists() {
        remove_file_if_present(socket_path)?;
        std::fs::rename(&fence, socket_path)
            .map_err(|error| format!("cannot restore legacy daemon socket: {error}"))?;
    }
    Ok(())
}

fn clear_stale_drain(runtime_dir: &Path) -> Result<(), String> {
    let path = runtime_dir.join(DRAIN_FILE);
    if let Some(document) = read_json_file(&path)? {
        if lease_is_live(&document) {
            return Err("refusing to remove a live test drain during recovery".to_owned());
        }
        remove_file_if_present(&path)?;
    }
    Ok(())
}

fn begin_drain(runtime_dir: &Path, owner: (u32, u32)) -> Result<HostDrain, String> {
    std::fs::create_dir_all(runtime_dir)
        .map_err(|error| format!("cannot create runtime directory: {error}"))?;
    let lock_path = runtime_dir.join(LOCK_FILE);
    let lock = unix_fs::open(
        &lock_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o666),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open test admission lock: {error}"))?;
    unix_fs::flock(&lock, FlockOperation::LockExclusive)
        .map_err(|error| format!("cannot lock test admission: {error}"))?;
    let drain_path = runtime_dir.join(DRAIN_FILE);
    if let Some(document) = read_json_file(&drain_path)? {
        if lease_is_live(&document) {
            return Err("another live test drain already exists".to_owned());
        }
        remove_file_if_present(&drain_path)?;
    }
    let process_start = process_start(std::process::id())
        .ok_or_else(|| "cannot identify installer process for drain lease".to_owned())?;
    let nonce = random_hex(16)?;
    let document = serde_json::json!({
        "schema":1,
        "pid":std::process::id(),
        "process_start":process_start,
        "nonce":nonce,
        "reason":"coordinator Rust cutover",
        "created_at":timestamp()?,
    });
    install::atomic_file(
        &drain_path,
        &serde_json::to_vec(&document).map_err(|error| error.to_string())?,
        0o600,
        Some(owner),
    )?;
    unix_fs::flock(&lock, FlockOperation::Unlock)
        .map_err(|error| format!("cannot unlock test admission: {error}"))?;
    Ok(HostDrain {
        runtime_dir: runtime_dir.to_owned(),
        nonce,
    })
}

fn end_drain(drain: &HostDrain) -> Result<(), String> {
    let lock = unix_fs::open(
        drain.runtime_dir.join(LOCK_FILE),
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open test admission lock: {error}"))?;
    unix_fs::flock(&lock, FlockOperation::LockExclusive)
        .map_err(|error| format!("cannot lock test admission: {error}"))?;
    let path = drain.runtime_dir.join(DRAIN_FILE);
    if read_json_file(&path)?
        .as_ref()
        .and_then(|document| document.get("nonce"))
        .and_then(Value::as_str)
        == Some(&drain.nonce)
    {
        remove_file_if_present(&path)?;
    }
    unix_fs::flock(&lock, FlockOperation::Unlock)
        .map_err(|error| format!("cannot unlock test admission: {error}"))
}

#[cfg(target_os = "linux")]
fn wait_for_zero_activity(runtime_dir: &Path) -> Result<(), String> {
    let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(format!(
            "cannot create activity watcher: {}",
            std::io::Error::last_os_error()
        ));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    let path = CString::new(runtime_dir.as_os_str().as_bytes())
        .map_err(|_| "runtime directory contains NUL".to_owned())?;
    let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
    if unsafe { libc::inotify_add_watch(descriptor.as_raw_fd(), path.as_ptr(), mask) } < 0 {
        return Err(format!(
            "cannot watch activity receipts: {}",
            std::io::Error::last_os_error()
        ));
    }
    loop {
        if read_activity_count(runtime_dir)? == 0 {
            return Ok(());
        }
        let mut events = [0u8; 65_536];
        let read = unsafe {
            libc::read(
                descriptor.as_raw_fd(),
                events.as_mut_ptr().cast(),
                events.len(),
            )
        };
        if read <= 0 {
            return Err(format!(
                "cannot wait for activity receipt: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn wait_for_zero_activity(_runtime_dir: &Path) -> Result<(), String> {
    Err("live cutover is supported only on Linux".to_owned())
}

fn read_activity_count(runtime_dir: &Path) -> Result<usize, String> {
    let document = read_json_file(&runtime_dir.join(ACTIVITY_FILE))?
        .ok_or_else(|| "test activity receipt is unavailable".to_owned())?;
    if document.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("test activity receipt has an unsupported schema".to_owned());
    }
    document
        .get("active")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| "test activity receipt has no active list".to_owned())
}

fn wait_for_bridge_connections(directory: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut delay = std::time::Duration::from_millis(25);
    loop {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("cannot inspect accepted sandbox requests".into()),
        };
        let mut active = false;
        for entry in entries {
            let entry = entry.map_err(|_| "cannot inspect accepted sandbox request")?;
            if entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "processing")
            {
                match read_json_file(&entry.path())? {
                    Some(value) if bridge_observation(&value) => {}
                    None => {}
                    _ => {
                        active = true;
                        break;
                    }
                }
            }
        }
        if !active {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                "accepted sandbox requests have not drained; no database backup was taken".into(),
            );
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(500));
    }
}

fn bridge_observation(value: &Value) -> bool {
    // Do not infer completion from age or from an arbitrary read-only label.
    // These two operations only observe state. In particular a health snapshot
    // stranded by ENOSPC cannot mutate authority during the cutover backup.
    let Ok(request) = serde_json::from_value::<devcoordinator2_api::RequestEnvelope>(value.clone())
    else {
        return false;
    };
    request.protocol == 2
        && !request.id.is_empty()
        && request.id.len() <= 64
        && request.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && matches!(request.operation.as_str(), "event.wait" | "health.summary")
        && devcoordinator2_api::operation(&request.operation).is_some_and(|operation| {
            operation.policy.read_only() && (operation.validate_params)(&request.params).is_ok()
        })
}

fn wait_for_socket_connections(
    runner: &dyn CommandRunner,
    ss: &Path,
    socket_path: &Path,
) -> Result<(), String> {
    let socket = path_text(socket_path)?;
    loop {
        let output = runner.run(&CommandRequest {
            program: ss.to_owned(),
            args: vec![
                "-xanH".into(),
                "src".into(),
                "=".into(),
                socket.clone().into(),
            ],
            environment: std::collections::BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            clear_environment: true,
        })?;
        if !output.success {
            return Err(command_failure(
                "inspect accepted daemon connections",
                &output.stderr,
                &output.stdout,
            ));
        }
        if output.stdout_truncated {
            return Err("accepted daemon connection inventory exceeded 64 KiB".to_owned());
        }
        let accepted = has_accepted_socket_connection(&output.stdout, &socket);
        if !accepted {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn has_accepted_socket_connection(output: &str, socket: &str) -> bool {
    output.lines().any(|line| {
        line.contains(socket)
            && line
                .split_whitespace()
                .any(|field| field.eq_ignore_ascii_case("ESTAB"))
    })
}

fn capture_entry(path: &Path) -> Result<SnapshotEntry, String> {
    let path_text = path_text(path)?;
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SnapshotEntry {
                path: path_text,
                kind: SnapshotKind::Missing,
                content_base64: None,
                link_target: None,
                mode: None,
                uid: None,
                gid: None,
            });
        }
        Err(error) => return Err(format!("cannot inspect installation target: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Ok(SnapshotEntry {
            path: path_text,
            kind: SnapshotKind::Symlink,
            content_base64: None,
            link_target: Some(path_text_value(
                &std::fs::read_link(path)
                    .map_err(|error| format!("cannot read installation link: {error}"))?,
            )?),
            mode: None,
            uid: None,
            gid: None,
        });
    }
    if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_BYTES {
        return Err("installation target is not a bounded regular file or symlink".to_owned());
    }
    let mut file = unix_fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open installation target: {error}"))?;
    let mut content = Vec::new();
    file.by_ref()
        .take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|error| format!("cannot capture installation target: {error}"))?;
    let after = file
        .metadata()
        .map_err(|error| format!("cannot recheck installation target: {error}"))?;
    if content.len() as u64 > MAX_SNAPSHOT_BYTES
        || file_identity(&metadata) != file_identity(&after)
        || content.len() as u64 != metadata.len()
    {
        return Err("installation target changed while it was captured".to_owned());
    }
    Ok(SnapshotEntry {
        path: path_text,
        kind: SnapshotKind::File,
        content_base64: Some(BASE64.encode(content)),
        link_target: None,
        mode: Some(metadata.mode() & 0o777),
        uid: Some(metadata.uid()),
        gid: Some(metadata.gid()),
    })
}

fn restore_entry(entry: &SnapshotEntry) -> Result<(), String> {
    let path = Path::new(&entry.path);
    match entry.kind {
        SnapshotKind::Missing => remove_file_if_present(path),
        SnapshotKind::Symlink => {
            remove_file_if_present(path)?;
            let target = entry
                .link_target
                .as_deref()
                .ok_or_else(|| "snapshot symlink has no target".to_owned())?;
            let parent = path
                .parent()
                .ok_or_else(|| "snapshot target has no parent".to_owned())?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create restored link parent: {error}"))?;
            let temporary = parent.join(format!(".restore-{}.tmp", random_hex(6)?));
            symlink(target, &temporary)
                .map_err(|error| format!("cannot recreate installation link: {error}"))?;
            std::fs::rename(temporary, path)
                .map_err(|error| format!("cannot restore installation link: {error}"))
        }
        SnapshotKind::File => {
            let content = entry
                .content_base64
                .as_deref()
                .ok_or_else(|| "snapshot file has no content".to_owned())?;
            let bytes = BASE64
                .decode(content)
                .map_err(|_| "snapshot file content is invalid".to_owned())?;
            install::atomic_file(
                path,
                &bytes,
                entry
                    .mode
                    .ok_or_else(|| "snapshot file has no mode".to_owned())?,
                Some((
                    entry
                        .uid
                        .ok_or_else(|| "snapshot file has no uid".to_owned())?,
                    entry
                        .gid
                        .ok_or_else(|| "snapshot file has no gid".to_owned())?,
                )),
            )
        }
    }
}

fn write_snapshot(
    snapshot: &InstallationSnapshot,
    status: &str,
    owner: (u32, u32),
) -> Result<(), String> {
    let mut document = snapshot.clone();
    document.status = status.to_owned();
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("cannot encode installation snapshot: {error}"))?;
    install::atomic_file(
        &Path::new(&snapshot.transaction_dir).join("installation-snapshot.json"),
        &bytes,
        0o600,
        Some(owner),
    )
}

fn replace_captured_target(path: &Path, source: &Path) -> Result<(), String> {
    remove_file_if_present(path)?;
    install::replace_direct_link(path, source)
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Err(format!(
            "refusing to remove directory at exact installation target {}",
            path.display()
        )),
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| format!("cannot remove exact installation target: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect exact installation target: {error}")),
    }
}

fn atomic_copy(source: &Path, destination: &Path, owner: (u32, u32)) -> Result<(), String> {
    let mut file = unix_fs::open(
        source,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| format!("cannot open database backup: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read database backup: {error}"))?;
    install::atomic_file(destination, &bytes, 0o600, Some(owner))
}

fn open_database_read_only(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .map_err(|error| format!("cannot open Coordinator database read-only: {error}"))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| format!("cannot protect Coordinator database read: {error}"))?;
    let schema = database_schema_from(&connection)?;
    if !(MINIMUM_SUPPORTED_DATABASE_SCHEMA..=DATABASE_SCHEMA_VERSION)
        .any(|supported| schema == supported.to_string())
    {
        return Err(format!(
            "cutover requires database schema {MINIMUM_SUPPORTED_DATABASE_SCHEMA} through {DATABASE_SCHEMA_VERSION}, found {schema}"
        ));
    }
    Ok(connection)
}

fn database_schema(path: &Path) -> Result<String, String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .map_err(|error| format!("cannot open database schema source: {error}"))?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| format!("cannot protect database schema read: {error}"))?;
    database_schema_from(&connection)
}

fn database_schema_from(connection: &Connection) -> Result<String, String> {
    connection
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("cannot read database schema: {error}"))
}

fn database_integrity(path: &Path) -> Result<bool, String> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect database integrity target: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("database integrity target is not a regular non-symlink file".to_owned());
    }
    let connection = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    ) {
        Ok(connection) => connection,
        Err(error) if sqlite_corruption(&error) => return Ok(false),
        Err(error) => return Err(format!("cannot open database for integrity check: {error}")),
    };
    let check = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0));
    let healthy = match check {
        Ok(value) => value == "ok",
        Err(error) if sqlite_corruption(&error) => false,
        Err(error) => return Err(format!("cannot check database integrity: {error}")),
    };
    if !healthy {
        return Ok(false);
    }
    match connection.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(schema) => Ok(schema.parse::<u32>().is_ok_and(|version| {
            (MINIMUM_SUPPORTED_DATABASE_SCHEMA..=DATABASE_SCHEMA_VERSION).contains(&version)
        })),
        Err(error) if sqlite_corruption(&error) => Ok(false),
        Err(error) => Err(format!("cannot confirm database schema: {error}")),
    }
}

fn sqlite_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(details, _)
            if matches!(
                details.code,
                rusqlite::ffi::ErrorCode::DatabaseCorrupt
                    | rusqlite::ffi::ErrorCode::NotADatabase
            )
    )
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

fn read_json_file(path: &Path) -> Result<Option<Value>, String> {
    let file = match unix_fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(format!("cannot open cutover state: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect cutover state: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
        return Err("cutover state is not a bounded regular file".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read cutover state: {error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("cutover state is invalid: {error}"))
}

fn lease_is_live(document: &Value) -> bool {
    document.get("schema").and_then(Value::as_u64) == Some(1)
        && document
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .zip(document.get("process_start").and_then(Value::as_str))
            .is_some_and(|(pid, expected)| process_start(pid).as_deref() == Some(expected))
}

fn process_start(pid: u32) -> Option<String> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let metadata = path.symlink_metadata().ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let tail = text.get(text.rfind(')')? + 2..)?;
    tail.split_whitespace().nth(19).map(str::to_owned)
}

fn timestamp() -> Result<String, String> {
    use time::{OffsetDateTime, macros::format_description};
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .map_err(|error| format!("cannot format cutover time: {error}"))
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut value = vec![0u8; bytes];
    getrandom::fill(&mut value)
        .map_err(|error| format!("cannot create cutover identity: {error}"))?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in value {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("string write");
    }
    Ok(encoded)
}

fn path_text(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("cutover paths must be absolute".to_owned());
    }
    path_text_value(path)
}

fn path_text_value(path: &Path) -> Result<String, String> {
    path.as_os_str()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "cutover paths must be valid UTF-8".to_owned())
}

fn command_failure(label: &str, stderr: &str, stdout: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!(
        "{label} failed: {}",
        if detail.is_empty() {
            "command exited nonzero"
        } else {
            detail
        }
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn bridge_observation_drain_keeps_mutations_and_unknown_requests_blocking() {
        let request = |operation: &str, params: Value| {
            serde_json::json!({
                "protocol":2,"id":"abcd","operation":operation,"params":params,"client":{}
            })
        };
        let health = request("health.summary", serde_json::json!({}));
        assert!(bridge_observation(&health));
        assert!(bridge_observation(&request(
            "event.wait",
            serde_json::json!({
                "filters":[{"filter_id":"upgrade","categories":["health"]}]
            })
        )));
        for operation in [
            "deployment.apply",
            "task.create",
            "test.start",
            "health.container_remove",
            "unknown",
            "plan.overview",
        ] {
            assert!(
                !bridge_observation(&request(operation, serde_json::json!({}))),
                "{operation}"
            );
        }
        assert!(!bridge_observation(&serde_json::json!({"request":health})));
        assert!(!bridge_observation(&request(
            "health.summary",
            serde_json::json!({"bad":true})
        )));
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            temporary.path().join("abcd.processing"),
            serde_json::to_vec(&health).unwrap(),
        )
        .unwrap();
        wait_for_bridge_connections(temporary.path()).unwrap();
        assert!(
            temporary.path().join("abcd.processing").exists(),
            "drain must preserve the client request"
        );
    }

    #[test]
    fn bridge_drain_waits_for_an_accepted_mutation_to_finish() {
        let temporary = tempfile::tempdir().unwrap();
        let pending = temporary.path().join("abcd.processing");
        std::fs::write(
            &pending,
            serde_json::to_vec(&serde_json::json!({
                "protocol":2,"id":"abcd","operation":"task.create",
                "params":{"title":"fixture task","kind":"improvement"},"client":{}
            }))
            .unwrap(),
        )
        .unwrap();
        let directory = temporary.path().to_owned();
        let drain = std::thread::spawn(move || wait_for_bridge_connections(&directory));
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(
            !drain.is_finished(),
            "accepted mutation was incorrectly bypassed"
        );
        assert!(pending.exists());
        std::fs::remove_file(pending).unwrap(); // Simulate only this fixture's completed response.
        drain.join().unwrap().unwrap();
    }
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::VecDeque;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        calls: Vec<&'static str>,
        applying: bool,
        activation: Result<Vec<String>, String>,
        integrity: Result<bool, String>,
        auxiliary_failures: VecDeque<&'static str>,
    }

    impl Fake {
        fn success() -> Self {
            Self {
                calls: Vec::new(),
                applying: false,
                activation: Ok(vec!["v2-cli".to_owned(), "restart-recovery".to_owned()]),
                integrity: Ok(true),
                auxiliary_failures: VecDeque::new(),
            }
        }

        fn called(&mut self, name: &'static str) -> Result<(), String> {
            self.calls.push(name);
            if self.auxiliary_failures.front() == Some(&name) {
                self.auxiliary_failures.pop_front();
                Err(format!("{name} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl CutoverAdapter for Fake {
        type Drain = String;
        type InstallationSnapshot = String;

        fn close_admission(&mut self) -> Result<Self::Drain, String> {
            self.called("close_admission")?;
            Ok("drain".to_owned())
        }
        fn wait_for_quiescence(&mut self, _: &Self::Drain) -> Result<(), String> {
            self.called("wait_for_quiescence")
        }
        fn wait_for_connections(&mut self) -> Result<(), String> {
            self.called("wait_for_connections")
        }
        fn deployment_is_applying(&mut self) -> Result<bool, String> {
            self.called("deployment_is_applying")?;
            Ok(self.applying)
        }
        fn backup_database(&mut self) -> Result<String, String> {
            self.called("backup_database")?;
            Ok("backup.sqlite3".to_owned())
        }
        fn capture_installation(&mut self) -> Result<Self::InstallationSnapshot, String> {
            self.called("capture_installation")?;
            Ok("snapshot".to_owned())
        }
        fn fence_legacy_socket(&mut self) -> Result<(), String> {
            self.called("fence_legacy_socket")
        }
        fn stop_legacy(&mut self) -> Result<(), String> {
            self.called("stop_legacy")
        }
        fn install_rust(&mut self) -> Result<(), String> {
            self.called("install_rust")
        }
        fn start_rust(&mut self) -> Result<(), String> {
            self.called("start_rust")
        }
        fn verify_live(&mut self) -> Result<Vec<String>, String> {
            self.calls.push("verify_live");
            self.activation.clone()
        }
        fn commit_activation(&mut self, _: &Self::InstallationSnapshot) -> Result<(), String> {
            self.called("commit_activation")
        }
        fn stop_rust(&mut self) -> Result<(), String> {
            self.called("stop_rust")
        }
        fn restore_installation(&mut self, _: &Self::InstallationSnapshot) -> Result<(), String> {
            self.called("restore_installation")
        }
        fn database_matches_backup(&mut self, _: &str) -> Result<bool, String> {
            self.called("database_matches_backup")?;
            self.integrity.clone()
        }
        fn restore_database(&mut self, _: &str) -> Result<(), String> {
            self.called("restore_database")
        }
        fn restore_legacy_socket(&mut self) -> Result<(), String> {
            self.called("restore_legacy_socket")
        }
        fn start_legacy(&mut self) -> Result<(), String> {
            self.called("start_legacy")
        }
        fn reopen_admission(&mut self, _: Self::Drain) -> Result<(), String> {
            self.called("reopen_admission")
        }
    }

    #[test]
    fn active_work_drains_before_the_endpoint_is_fenced() {
        let mut fake = Fake::success();
        activate(&mut fake).unwrap();
        let drained = fake
            .calls
            .iter()
            .position(|call| *call == "wait_for_quiescence")
            .unwrap();
        let fenced = fake
            .calls
            .iter()
            .position(|call| *call == "fence_legacy_socket")
            .unwrap();
        assert!(
            drained < fenced,
            "API must stay reachable while accepted work drains"
        );
    }

    #[test]
    fn failed_test_drain_never_hides_the_api() {
        let mut fake = Fake::success();
        fake.auxiliary_failures.push_back("wait_for_quiescence");
        assert!(activate(&mut fake).is_err());
        assert_eq!(
            fake.calls,
            ["close_admission", "wait_for_quiescence", "reopen_admission"]
        );
    }

    #[test]
    fn failed_connection_drain_restores_the_endpoint_before_reopening() {
        let mut fake = Fake::success();
        fake.auxiliary_failures.push_back("wait_for_connections");
        assert!(activate(&mut fake).is_err());
        assert!(fake.calls.ends_with(&[
            "wait_for_connections",
            "restore_legacy_socket",
            "reopen_admission"
        ]));
        assert!(!fake.calls.contains(&"backup_database"));
    }

    #[test]
    fn successful_cutover_orders_every_safety_gate_before_activation() {
        let mut fake = Fake::success();
        let receipt = activate(&mut fake).unwrap();
        assert_eq!(receipt.status, "activated");
        assert_eq!(receipt.checks, ["v2-cli", "restart-recovery"]);
        assert_eq!(
            fake.calls,
            [
                "close_admission",
                "wait_for_quiescence",
                "fence_legacy_socket",
                "wait_for_connections",
                "deployment_is_applying",
                "backup_database",
                "capture_installation",
                "stop_legacy",
                "install_rust",
                "start_rust",
                "verify_live",
                "commit_activation",
                "reopen_admission",
            ]
        );
    }

    #[test]
    fn applying_deployment_blocks_before_backup_or_runtime_mutation() {
        let mut fake = Fake::success();
        fake.applying = true;
        assert!(
            activate(&mut fake)
                .unwrap_err()
                .contains("deployment is applying")
        );
        assert_eq!(
            fake.calls,
            [
                "close_admission",
                "wait_for_quiescence",
                "fence_legacy_socket",
                "wait_for_connections",
                "deployment_is_applying",
                "restore_legacy_socket",
                "reopen_admission"
            ]
        );
    }

    #[test]
    fn ordinary_activation_failure_preserves_intact_same_schema_data() {
        let mut fake = Fake::success();
        fake.activation = Err("acceptance failed".to_owned());
        let error = activate(&mut fake).unwrap_err();
        assert!(error.contains("without replacing the intact database"));
        assert!(!fake.calls.contains(&"restore_database"));
        assert!(fake.calls.ends_with(&[
            "database_matches_backup",
            "restore_legacy_socket",
            "start_legacy",
            "reopen_admission"
        ]));
    }

    #[test]
    fn failed_integrity_or_changed_schema_restores_the_private_backup() {
        let mut fake = Fake::success();
        fake.activation = Err("daemon failed".to_owned());
        fake.integrity = Ok(false);
        let error = activate(&mut fake).unwrap_err();
        assert!(error.contains("and database backup"));
        let restore = fake
            .calls
            .iter()
            .position(|call| *call == "restore_database")
            .unwrap();
        let restart = fake
            .calls
            .iter()
            .position(|call| *call == "start_legacy")
            .unwrap();
        assert!(restore < restart);
    }

    #[test]
    fn partial_legacy_stop_restores_socket_and_restarts_prior_units() {
        let mut fake = Fake::success();
        fake.auxiliary_failures.push_back("stop_legacy");
        let error = activate(&mut fake).unwrap_err();
        assert!(error.contains("stop_legacy failed"));
        assert!(fake.calls.contains(&"restore_legacy_socket"));
        assert!(fake.calls.contains(&"start_legacy"));
        assert!(!fake.calls.contains(&"install_rust"));
        assert_eq!(fake.calls.last(), Some(&"reopen_admission"));
    }

    #[test]
    fn accepted_connection_detection_ignores_the_fenced_listener() {
        let socket = "/run/devcoordinator2/daemon.sock";
        assert!(!has_accepted_socket_connection(
            "u_str LISTEN 0 128 /run/devcoordinator2/daemon.sock 1 * 0\n",
            socket
        ));
        assert!(has_accepted_socket_connection(
            "u_str ESTAB 0 0 /run/devcoordinator2/daemon.sock 1 * 2\n",
            socket
        ));
    }

    #[test]
    fn socket_wait_queries_only_the_daemon_endpoint() {
        let runner = HostFake::default();
        wait_for_socket_connections(
            &runner,
            Path::new("/usr/bin/ss"),
            Path::new("/run/devcoordinator2/daemon.sock"),
        )
        .unwrap();

        let requests = runner.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].args,
            [
                OsString::from("-xanH"),
                OsString::from("src"),
                OsString::from("="),
                OsString::from("/run/devcoordinator2/daemon.sock"),
            ]
        );
    }

    #[derive(Default)]
    struct HostFake {
        commit: String,
        fail_ping: bool,
        retryable_ping_failures: AtomicUsize,
        requests: Mutex<Vec<CommandRequest>>,
    }

    impl CommandRunner for HostFake {
        fn run(&self, request: &CommandRequest) -> Result<crate::install::CommandOutput, String> {
            self.requests.lock().unwrap().push(request.clone());
            if request.args == [OsString::from("--source-commit")] {
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: self.commit.clone(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.args == [OsString::from("ping")] {
                if self
                    .retryable_ping_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Ok(crate::install::CommandOutput {
                        success: false,
                        stdout: "{\"protocol\":2,\"id\":\"x\",\"ok\":false,\"error\":{\"code\":\"daemon_unavailable\",\"message\":\"not ready\",\"detail\":\"\"}}\n".to_owned(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    });
                }
                return Ok(crate::install::CommandOutput {
                    success: !self.fail_ping,
                    stdout: if self.fail_ping {
                        String::new()
                    } else {
                        "{\"protocol\":2,\"id\":\"x\",\"ok\":true,\"data\":{}}\n".to_owned()
                    },
                    stderr: if self.fail_ping {
                        "ping failed".to_owned()
                    } else {
                        String::new()
                    },
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            Ok(crate::install::CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    struct HostWorld {
        _temporary: tempfile::TempDir,
        _listener: UnixListener,
        config: HostCutoverConfig,
        expected_owner: (u32, u32),
        old_daemon: String,
        old_cli: String,
        commit: String,
    }

    fn host_world() -> HostWorld {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let source = root.join("source");
        std::fs::create_dir_all(source.join("deploy")).unwrap();
        std::fs::create_dir_all(source.join("edge")).unwrap();
        std::fs::create_dir_all(source.join("target/release")).unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.tmpfiles.conf"),
            include_str!("../../../deploy/devcoordinator2.tmpfiles.conf"),
        )
        .unwrap();
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
        let commit = "c".repeat(40);
        let mut receipts = Vec::new();
        for name in [
            "devcoordinator2",
            "devcoordinator2-executor",
            "devcoordinator2-tooling",
        ] {
            let path = source.join("target/release").join(name);
            let bytes = format!("binary-{name}").into_bytes();
            std::fs::write(&path, &bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            receipts.push(crate::install::BinaryReceipt {
                name: name.to_owned(),
                path: path.display().to_string(),
                sha256: {
                    let digest = Sha256::digest(&bytes);
                    digest
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join("")
                },
                bytes: bytes.len() as u64,
                source_commit: commit.clone(),
            });
        }
        let manifest =
            crate::install::manifest(&source, &commit, "2026-09-04T00:00:00Z", receipts).unwrap();
        let expected_owner = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let candidate_manifest = root.join("etc/candidate.json");
        crate::install::write_manifest(&candidate_manifest, &manifest, expected_owner).unwrap();
        let runtime_dir = root.join("run");
        std::fs::create_dir(&runtime_dir).unwrap();
        std::fs::write(
            runtime_dir.join(ACTIVITY_FILE),
            b"{\"schema\":1,\"generation\":1,\"active\":[]}",
        )
        .unwrap();
        let socket_path = runtime_dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let state = root.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir_all(state.join("public")).unwrap();
        std::fs::create_dir_all(root.join("edge-state")).unwrap();
        let routes = serde_json::json!({"schema":2,"generation":100,"payload_sha256":"f".repeat(64),"routes":[]});
        std::fs::write(
            state.join("public/routes.json"),
            serde_json::to_vec(&routes).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("edge-state/routes.accepted.json"),
            serde_json::to_vec(&routes).unwrap(),
        )
        .unwrap();
        let database_path = state.join("authority.sqlite3");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(include_str!("../../control/src/schema.sql"))
            .unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES('schema_version','15')",
                [],
            )
            .unwrap();
        drop(connection);
        let system = root.join("system");
        let bin = root.join("bin");
        std::fs::create_dir(&system).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let daemon_unit_path = system.join("devcoordinator2.service");
        let edge_unit_path = system.join("devcoordinator2-edge.service");
        let cli_link = bin.join("devcoordinator2");
        let tooling_link = bin.join("devcoordinator2-tooling");
        let installed_manifest = root.join("etc/install-manifest.json");
        let old_daemon = "ExecStart=/usr/bin/python3 -m devcoordinator2.daemon\n".to_owned();
        let old_cli = "#!/usr/bin/python3\n".to_owned();
        std::fs::write(&daemon_unit_path, &old_daemon).unwrap();
        std::fs::write(&edge_unit_path, "old edge\n").unwrap();
        std::fs::write(&cli_link, &old_cli).unwrap();
        std::fs::write(&tooling_link, "old tooling\n").unwrap();
        std::fs::write(&installed_manifest, "old manifest\n").unwrap();
        HostWorld {
            config: HostCutoverConfig {
                candidate_manifest,
                installed_manifest,
                tmpfiles_path: root.join("etc/tmpfiles.conf"),
                transaction_dir: root.join("transaction"),
                runtime_dir,
                socket_path,
                database_path,
                daemon_unit_path,
                edge_unit_path,
                cli_link,
                tooling_link,
                systemctl: "/fake/systemctl".into(),
                ss: "/fake/ss".into(),
                instance_env: root.join("etc/instance.env"),
                daemon_unit: "devcoordinator2.service".to_owned(),
                edge_unit: "devcoordinator2-edge.service".to_owned(),
                edge_state_dir: root.join("edge-state"),
                canary: false,
            },
            expected_owner,
            old_daemon,
            old_cli,
            commit,
            _listener: listener,
            _temporary: temporary,
        }
    }

    #[test]
    fn concrete_host_adapter_installs_verified_files_and_commits_snapshot() {
        let world = host_world();
        let prior_rule =
            "d /var/lib/devcoordinator2 0750 root root -\n# retain this instance setting\n";
        std::fs::write(&world.config.tmpfiles_path, prior_rule).unwrap();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        let receipt = activate(&mut host).unwrap();
        assert_eq!(receipt.status, "activated");
        assert_eq!(
            std::fs::metadata(&world.config.database_path)
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(world.config.database_path.parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o751
        );
        assert_eq!(
            std::fs::read_to_string(&world.config.tmpfiles_path).unwrap(),
            prior_rule.replace("0750", "0751")
        );
        assert!(Path::new(&receipt.backup).is_file());
        assert!(
            std::fs::read_to_string(&world.config.daemon_unit_path)
                .unwrap()
                .contains("target/release/devcoordinator2 daemon")
        );
        assert!(world.config.cli_link.is_symlink());
        let snapshot: InstallationSnapshot = serde_json::from_slice(
            &std::fs::read(
                world
                    .config
                    .transaction_dir
                    .join("installation-snapshot.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.status, "committed");
        assert!(
            !world
                .config
                .runtime_dir
                .join("daemon.pre-cutover.sock")
                .exists()
        );
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
    }

    #[test]
    fn concrete_host_adapter_activates_supported_schemas_and_rejects_unknown_schemas() {
        for schema in 15..=DATABASE_SCHEMA_VERSION {
            let world = host_world();
            let connection = Connection::open(&world.config.database_path).unwrap();
            connection
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='schema_version'",
                    [schema.to_string()],
                )
                .unwrap();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();

            let receipt = activate(&mut host).unwrap();
            assert_eq!(receipt.status, "activated");
            assert_eq!(
                database_schema(&world.config.database_path).unwrap(),
                schema.to_string()
            );
            for unsupported in [
                "14".to_owned(),
                (DATABASE_SCHEMA_VERSION + 1).to_string(),
                "invalid".to_owned(),
            ] {
                connection
                    .execute(
                        "UPDATE meta SET value=?1 WHERE key='schema_version'",
                        [unsupported],
                    )
                    .unwrap();
                assert!(open_database_read_only(&world.config.database_path).is_err());
                assert!(!database_integrity(&world.config.database_path).unwrap());
            }
        }
    }

    #[test]
    fn concrete_host_adapter_waits_for_the_daemon_socket_to_bind() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            retryable_ping_failures: AtomicUsize::new(1),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();

        let receipt = activate(&mut host).unwrap();

        assert_eq!(receipt.status, "activated");
        assert_eq!(
            runner
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.args == [OsString::from("ping")])
                .count(),
            2
        );
    }

    #[test]
    fn concrete_host_adapter_requires_backup_restore_after_schema_upgrade() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        std::fs::create_dir_all(&world.config.transaction_dir).unwrap();
        let backup = world
            .config
            .transaction_dir
            .join("authority-before.sqlite3");
        std::fs::copy(&world.config.database_path, &backup).unwrap();
        let connection = Connection::open(&world.config.database_path).unwrap();
        connection
            .execute("UPDATE meta SET value='16' WHERE key='schema_version'", [])
            .unwrap();
        drop(connection);

        assert!(
            !host
                .database_matches_backup(backup.to_str().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn failed_commit_keeps_new_user_writes_fenced() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        host.backup_database().unwrap();
        let snapshot = host.capture_installation().unwrap();
        host.fence_legacy_socket().unwrap();
        let record = world
            .config
            .transaction_dir
            .join("installation-snapshot.json");
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir(&record).unwrap();
        assert!(host.commit_activation(&snapshot).is_err());
        assert!(
            world
                .config
                .runtime_dir
                .join("daemon.pre-cutover.sock")
                .exists()
        );
    }

    #[test]
    fn recovery_checks_owned_listeners_and_exact_external_reservations() {
        let world = host_world();
        let managed_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let managed_port = managed_listener.local_addr().unwrap().port();
        let external_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let external_port = external_listener.local_addr().unwrap().port();
        drop(external_listener);
        let connection = Connection::open(&world.config.database_path).unwrap();
        connection.execute_batch("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r','/fixture','Fixture','now',1000,'now');
            INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w','r','/fixture','now','now');
            INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,current_generation,created_at,created_by_uid,client,updated_at) VALUES('managed','r','w','web','worktree','f','{}','running',1,'now',1000,'other','now');
            INSERT INTO observed_deployments(observed_deployment_id,repository_id,name,native_project,state,health,source,evidence_json,observed_at,imported_at) VALUES('external','r','external','external','stopped','unknown','fixture','{}','now','now');").unwrap();
        connection.execute("INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(?1,'managed','web',0,'now','lease-managed')", [managed_port]).unwrap();
        connection.execute("INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES('managed','managed','web',?1,1,'now','lease-managed')", [managed_port]).unwrap();
        connection.execute("INSERT INTO observed_routes(domain,observed_deployment_id,component,port,public,evidence_json,observed_at) VALUES('external','external','app',?1,0,'{}','now')", [external_port]).unwrap();
        drop(connection);
        let mut routes = serde_json::json!([
            {"label":"managed","deployment_id":"managed","component":"web","port":managed_port,"lease_id":"lease-managed"},
            {"label":"external","deployment_id":"external","component":"app","port":external_port,"observed":true}
        ]);
        let publish = |routes: &Value| {
            let doc = serde_json::json!({"schema":2,"generation":101,"payload_sha256":"a".repeat(64),"routes":routes});
            let bytes = serde_json::to_vec(&doc).unwrap();
            std::fs::write(
                world
                    .config
                    .database_path
                    .parent()
                    .unwrap()
                    .join("public/routes.json"),
                &bytes,
            )
            .unwrap();
            std::fs::write(
                world.config.edge_state_dir.join("routes.accepted.json"),
                &bytes,
            )
            .unwrap();
        };
        let verify =
            || verify_reconciled_routes(&world.config.database_path, &world.config.edge_state_dir);
        publish(&routes);
        verify().expect("offline external application does not block Coordinator");
        routes[1]["port"] = serde_json::json!(managed_port);
        publish(&routes);
        assert!(verify().unwrap_err().contains("reservation mismatch"));
        routes[1]["port"] = serde_json::json!(external_port);
        routes[0]["label"] = serde_json::json!("external");
        publish(&routes);
        assert!(verify().unwrap_err().contains("lease ownership mismatch"));
        routes[0]["label"] = serde_json::json!("managed");
        routes[0]["observed"] = serde_json::json!(true);
        publish(&routes);
        assert!(verify().unwrap_err().contains("reservation mismatch"));
        routes[0].as_object_mut().unwrap().remove("observed");
        drop(managed_listener);
        publish(&routes);
        assert!(verify().unwrap_err().contains("listener is unavailable"));
    }

    #[test]
    fn all_supported_schemas_preserve_intact_data_after_failed_activation() {
        for schema in MINIMUM_SUPPORTED_DATABASE_SCHEMA..=DATABASE_SCHEMA_VERSION {
            let world = host_world();
            let connection = Connection::open(&world.config.database_path).unwrap();
            connection
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='schema_version'",
                    [schema.to_string()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO meta(key,value) VALUES('preserve-on-failure','current-data')",
                    [],
                )
                .unwrap();
            drop(connection);
            assert!(database_integrity(&world.config.database_path).unwrap());
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fail_ping: true,
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
            let error = activate(&mut host).unwrap_err();
            assert!(
                error.contains("without replacing the intact database"),
                "schema {schema}: {error}"
            );
            let backup = world
                .config
                .transaction_dir
                .join("authority-before.sqlite3");
            assert!(database_integrity(&backup).unwrap());
            assert_eq!(
                database_schema(&world.config.database_path).unwrap(),
                schema.to_string()
            );
            let connection = Connection::open(&world.config.database_path).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT value FROM meta WHERE key='preserve-on-failure'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "current-data"
            );
        }
    }

    #[test]
    fn concrete_host_adapter_restores_python_files_and_socket_on_failed_ping() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fail_ping: true,
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        let error = activate(&mut host).unwrap_err();
        assert!(error.contains("without replacing the intact database"));
        assert_eq!(
            std::fs::read_to_string(&world.config.daemon_unit_path).unwrap(),
            world.old_daemon
        );
        assert_eq!(
            std::fs::read_to_string(&world.config.cli_link).unwrap(),
            world.old_cli
        );
        assert!(world.config.socket_path.exists());
        let snapshot: InstallationSnapshot = serde_json::from_slice(
            &std::fs::read(
                world
                    .config
                    .transaction_dir
                    .join("installation-snapshot.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.status, "rolled_back");
        assert!(database_integrity(&world.config.database_path).unwrap());
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
    }

    #[test]
    fn recovery_refuses_missing_old_and_terminal_targets_before_system_changes() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..HostFake::default()
        });
        let missing = RecoveryConfig::default();
        assert!(
            recover_host(&missing, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .contains("target")
        );
        assert!(runner.requests.lock().unwrap().is_empty());
        let unavailable = RecoveryConfig {
            transaction_dir: world.config.transaction_dir.join("missing"),
            ..RecoveryConfig::default()
        };
        assert!(
            recover_host(&unavailable, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .starts_with("recovery target invalid:")
        );
        assert!(runner.requests.lock().unwrap().is_empty());
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        host.backup_database().unwrap();
        let snapshot = host.capture_installation().unwrap();
        write_snapshot(&snapshot, "committed", world.expected_owner).unwrap();
        runner.requests.lock().unwrap().clear();
        let config = RecoveryConfig {
            transaction_dir: world.config.transaction_dir.clone(),
            runtime_dir: world.config.runtime_dir.clone(),
            socket_path: world.config.socket_path.clone(),
            database_path: world.config.database_path.clone(),
            systemctl: world.config.systemctl.clone(),
            daemon_unit: world.config.daemon_unit.clone(),
            edge_unit: world.config.edge_unit.clone(),
            edge_state_dir: world.config.edge_state_dir.clone(),
        };
        assert!(
            recover_host(&config, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .contains("in-progress")
        );
        assert!(runner.requests.lock().unwrap().is_empty());
        write_snapshot(&snapshot, "prepared", world.expected_owner).unwrap();
        std::fs::write(
            world
                .config
                .transaction_dir
                .join("authority-before.sqlite3"),
            b"not the captured backup",
        )
        .unwrap();
        assert!(
            recover_host(&config, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .contains("backup")
        );
        assert!(runner.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn offline_recovery_replays_persisted_snapshot_after_interrupted_install() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        let drain = host.close_admission().unwrap();
        host.fence_legacy_socket().unwrap();
        host.wait_for_quiescence(&drain).unwrap();
        host.backup_database().unwrap();
        host.capture_installation().unwrap();
        host.stop_legacy().unwrap();
        host.install_rust().unwrap();
        std::fs::write(&world.config.database_path, b"not a database").unwrap();
        std::fs::write(
            world.config.runtime_dir.join(DRAIN_FILE),
            b"{\"schema\":1,\"pid\":4294967295,\"process_start\":\"gone\",\"nonce\":\"stale\"}",
        )
        .unwrap();
        drop(host);

        let recovery = recover_host(
            &RecoveryConfig {
                transaction_dir: world.config.transaction_dir.clone(),
                runtime_dir: world.config.runtime_dir.clone(),
                socket_path: world.config.socket_path.clone(),
                database_path: world.config.database_path.clone(),
                systemctl: world.config.systemctl.clone(),
                daemon_unit: world.config.daemon_unit.clone(),
                edge_unit: world.config.edge_unit.clone(),
                edge_state_dir: world.config.edge_state_dir.clone(),
            },
            runner.as_ref(),
            world.expected_owner,
        )
        .unwrap();
        assert_eq!(recovery.status, "recovered");
        assert!(recovery.database_restored);
        assert_eq!(
            std::fs::read_to_string(&world.config.daemon_unit_path).unwrap(),
            world.old_daemon
        );
        assert_eq!(
            std::fs::read_to_string(&world.config.cli_link).unwrap(),
            world.old_cli
        );
        assert!(world.config.socket_path.exists());
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        let snapshot: InstallationSnapshot = serde_json::from_slice(
            &std::fs::read(
                world
                    .config
                    .transaction_dir
                    .join("installation-snapshot.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.status, "recovered");
    }
}
