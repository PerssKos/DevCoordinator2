//! Fenced PostgreSQL activation, native upgrades, and delayed source retirement.
//!
//! Concrete host access is an adapter for complete rollback rehearsals. Each
//! attempt copies into a fresh database while the prior authority stays intact.
//! Before write admission, failure restores the prior service/configuration;
//! after admission, recovery preserves PostgreSQL writes and moves forward.
//! Final source retirement requires separate post-activation live acceptance.

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
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
    render_daemon_unit_for_binary, render_edge_unit, render_tests_slice_unit,
};

#[path = "cutover/finalize.rs"]
mod finalize;
#[path = "cutover/native_recovery.rs"]
mod native_recovery;
#[cfg(feature = "selftest-fixtures")]
#[path = "cutover/rehearsal.rs"]
pub mod rehearsal;
mod startup_guard;
pub use finalize::{FinalizationConfig, FinalizationReceipt, LiveAcceptance, finalize_host};

const DRAIN_FILE: &str = "test-drain.json";
const ACTIVITY_FILE: &str = "test-activity.json";
const LOCK_FILE: &str = "test-admission.lock";
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;
const MINIMUM_SUPPORTED_DATABASE_SCHEMA: u32 = 15;
const MAXIMUM_LEGACY_SQLITE_SCHEMA: u32 = 34;

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
    fn writes_admitted(&self) -> bool {
        false
    }
    fn finalization_required(&self) -> bool {
        false
    }
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
    // The legacy process and its background writers stop before the frozen
    // authority copy is captured. Accepted work and socket users already drained.
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
    let backup = match adapter.backup_database() {
        Ok(backup) => backup,
        Err(error) => {
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
    };
    let snapshot = match adapter.capture_installation() {
        Ok(snapshot) => snapshot,
        Err(error) => {
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
    };
    let activation = (|| {
        adapter.install_rust()?;
        adapter.start_rust()?;
        let checks = adapter.verify_live()?;
        adapter.commit_activation(&snapshot)?;
        Ok(checks)
    })();
    match activation {
        Ok(checks) => Ok(CutoverReceipt {
            status: if adapter.finalization_required() {
                "activated_awaiting_final_acceptance"
            } else {
                "activated"
            }
            .to_owned(),
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
    if adapter.writes_admitted() {
        return Err(format!(
            "PostgreSQL authority is active; recovery must preserve its new writes and roll forward: {activation_error}"
        ));
    }
    let mut failures = Vec::new();
    if let Err(error) = adapter.stop_rust() {
        return Err(format!(
            "Rust activation failed: {activation_error}; cannot stop candidate writers: {error}; restoration was not attempted"
        ));
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
    if failures.is_empty()
        && let Err(error) = adapter.start_legacy()
    {
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
    pub candidate_acceptance: PathBuf,
    pub postgres: install::postgres::PostgresSetup,
    pub candidate_manifest: PathBuf,
    pub installed_manifest: PathBuf,
    pub source_cache_dir: PathBuf,
    pub tmpfiles_path: PathBuf,
    pub transaction_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub database_path: PathBuf,
    pub daemon_unit_path: PathBuf,
    pub edge_unit_path: PathBuf,
    pub edge_env: PathBuf,
    pub tests_slice_unit_path: PathBuf,
    pub cli_link: PathBuf,
    pub tooling_link: PathBuf,
    pub systemctl: PathBuf,
    pub ss: PathBuf,
    pub instance_env: PathBuf,
    pub daemon_unit: String,
    pub edge_unit: String,
    pub edge_state_dir: PathBuf,
    pub canary: bool,
    /// Runtime-only, private-network service units for an owned integration fixture.
    pub ephemeral_services: bool,
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
    pub source_root: PathBuf,
    pub edge_env: PathBuf,
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
            source_root: "/home/DevCoordinator2".into(),
            edge_env: "/etc/devcoordinator2/edge.env".into(),
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
            candidate_acceptance: "/etc/devcoordinator2/candidate-acceptance.json".into(),
            postgres: install::postgres::PostgresSetup::default(),
            candidate_manifest: "/etc/devcoordinator2/candidate-install-manifest.json".into(),
            installed_manifest: "/etc/devcoordinator2/install-manifest.json".into(),
            source_cache_dir: "/var/lib/devcoordinator2-sources".into(),
            tmpfiles_path: "/etc/tmpfiles.d/devcoordinator2.conf".into(),
            transaction_dir: "/var/lib/devcoordinator2/cutover/postgres-v3".into(),
            runtime_dir: "/run/devcoordinator2".into(),
            socket_path: "/run/devcoordinator2/daemon.sock".into(),
            database_path: "/var/lib/devcoordinator2/authority.sqlite3".into(),
            daemon_unit_path: "/etc/systemd/system/devcoordinator2.service".into(),
            edge_unit_path: "/etc/systemd/system/devcoordinator2-edge.service".into(),
            edge_env: "/etc/devcoordinator2/edge.env".into(),
            tests_slice_unit_path: "/etc/systemd/system/devcoordinator2-tests.slice".into(),
            cli_link: "/usr/local/bin/devcoordinator2".into(),
            tooling_link: "/usr/local/bin/devcoordinator2-tooling".into(),
            systemctl: "/usr/bin/systemctl".into(),
            ss: "/usr/bin/ss".into(),
            instance_env: "/etc/devcoordinator2/instance.env".into(),
            daemon_unit: "devcoordinator2.service".to_owned(),
            edge_unit: "devcoordinator2-edge.service".to_owned(),
            edge_state_dir: "/var/lib/devcoordinator2-edge".into(),
            canary: false,
            ephemeral_services: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSnapshot {
    pub schema: u8,
    #[serde(default)]
    pub bootstrap: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityKind {
    LegacySqlite,
    Postgres,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorEdgeSource {
    source: install::edge_source::PinnedSource,
    installed_manifest: PathBuf,
    manifest_sha256: String,
    edge_unit_path: PathBuf,
    edge_env: PathBuf,
}

pub struct HostCutover {
    authority: AuthorityKind,
    authority_marker_path: PathBuf,
    frozen_companions: Vec<(PathBuf, String)>,
    inherited_retirement_targets: Vec<Value>,
    config: HostCutoverConfig,
    runner: Arc<dyn CommandRunner>,
    manifest: InstallManifest,
    daemon_unit_text: String,
    edge_unit_text: String,
    tests_slice_unit_text: String,
    tmpfiles_text: String,
    expected_owner: (u32, u32),
    fenced: bool,
    admitted: bool,
    acceptance_sha256: String,
    frozen_source_sha256: Option<String>,
    verified_backup_sha256: Option<String>,
    postgres_started: bool,
    prior_edge_source: Option<install::edge_source::PinnedSource>,
    #[cfg(feature = "selftest-fixtures")]
    rehearsal_inputs: bool,
    #[cfg(feature = "selftest-fixtures")]
    rehearsal_fail_edge_start: bool,
}

impl HostCutover {
    fn capture_installation_for(&self, bootstrap: bool) -> Result<InstallationSnapshot, String> {
        let entries = self
            .snapshot_targets()
            .into_iter()
            .map(capture_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let backup = self.config.transaction_dir.join("authority-before.sqlite3");
        let backup_sha256 = if bootstrap
            && !self
                .config
                .database_path
                .try_exists()
                .map_err(|error| error.to_string())?
        {
            if backup.symlink_metadata().is_ok() {
                return Err("unexpected backup for database-free bootstrap".into());
            }
            String::new()
        } else {
            install::hash_file(&backup)?
        };
        let snapshot = InstallationSnapshot {
            schema: 2,
            bootstrap,
            status: "prepared".to_owned(),
            transaction_dir: path_text(&self.config.transaction_dir)?,
            entries,
            backup_sha256,
            candidate_commit: self.manifest.source_commit.clone(),
        };
        write_snapshot(&snapshot, "prepared", self.expected_owner)?;
        Ok(snapshot)
    }

    fn backup_legacy_database(&mut self) -> Result<String, String> {
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
        drop(connection);
        self.frozen_source_sha256 = Some(install::hash_file(&self.config.database_path)?);
        for suffix in ["-wal", "-shm", "-journal"] {
            let path = PathBuf::from(format!("{}{}", self.config.database_path.display(), suffix));
            if let Ok(metadata) = path.symlink_metadata() {
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.uid() != self.expected_owner.0
                {
                    return Err("frozen SQLite companion is not an owned regular file".into());
                }
                self.frozen_companions
                    .push((path.clone(), install::hash_file(&path)?));
            }
        }

        self.verified_backup_sha256 = Some(install::hash_file(&backup)?);
        path_text(&backup)
    }

    pub fn bootstrap(&mut self) -> Result<CutoverReceipt, String> {
        if !self.config.canary {
            return Err("first installation requires --canary".to_owned());
        }
        // Initial configuration is prepared before bootstrap and must survive
        // rollback. Its inclusion in the snapshot is not an installed service.
        for path in self
            .snapshot_targets()
            .into_iter()
            .filter(|path| *path != self.config.edge_env.as_path())
            .chain([
                self.config.socket_path.as_path(),
                self.config
                    .runtime_dir
                    .join("daemon.pre-cutover.sock")
                    .as_path(),
            ])
        {
            match path.symlink_metadata() {
                Ok(_) => {
                    return Err(format!(
                        "bootstrap requires absent installation target: {}",
                        path.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot inspect bootstrap target: {error}")),
            }
        }
        for unit in [&self.config.daemon_unit, &self.config.edge_unit] {
            let output = self.runner.run(&CommandRequest {
                program: self.config.systemctl.clone(),
                args: ["show", "--property=LoadState", "--value", unit]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                environment: Default::default(),
                clear_environment: true,
            })?;
            if output.stdout.trim() != "not-found" {
                return Err(format!("bootstrap requires an uninstalled unit: {unit}"));
            }
        }
        let drain = self.close_admission()?;
        let outcome = self.bootstrap_drained();
        let reopen = self.reopen_admission(drain);
        match outcome {
            Ok(receipt) => reopen.map(|()| receipt),
            Err(error) => Err(combine(error, "reopen admission", reopen)),
        }
    }

    fn bootstrap_drained(&mut self) -> Result<CutoverReceipt, String> {
        let mut lock_name = self
            .config
            .socket_path
            .file_name()
            .ok_or("socket filename is missing")?
            .to_os_string();
        lock_name.push(".lock");
        let endpoint_lock = File::from(
            unix_fs::open(
                self.config.socket_path.with_file_name(lock_name),
                OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| format!("cannot open bootstrap endpoint lease: {error}"))?,
        );
        unix_fs::flock(&endpoint_lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|error| format!("bootstrap requires no live daemon: {error}"))?;
        if self.config.socket_path.symlink_metadata().is_ok() {
            return Err("daemon socket appeared during bootstrap preparation".to_owned());
        }
        if read_json_file(&self.config.runtime_dir.join(ACTIVITY_FILE))?.is_some()
            && read_activity_count(&self.config.runtime_dir)? != 0
        {
            return Err(
                "bootstrap blocked by existing test activity; recover prior tests first".to_owned(),
            );
        }
        let backup = if self
            .config
            .database_path
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            if !database_integrity(&self.config.database_path)? {
                return Err("bootstrap database failed integrity verification".to_owned());
            }
            if self.deployment_is_applying()? {
                return Err("bootstrap blocked while a deployment is applying".to_owned());
            }
            // Bootstrap has no installed services to guard. The absent-unit
            // checks and exclusive endpoint lease above protect this read.
            self.backup_legacy_database()?
        } else {
            install::ensure_owned_directory(
                &self.config.transaction_dir,
                0o700,
                self.expected_owner,
            )?;
            String::new()
        };
        let snapshot = self.capture_installation_for(true)?;
        let activation = (|| {
            // The candidate must not admit writes before its first verified
            // activation, even though there is no old daemon socket to rename.
            let fence = self.config.runtime_dir.join("daemon.pre-cutover.sock");
            let _fence_listener = std::os::unix::net::UnixListener::bind(&fence)
                .map_err(|error| format!("cannot fence bootstrap write admission: {error}"))?;
            std::fs::set_permissions(&fence, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("cannot protect bootstrap admission fence: {error}"))?;
            let binding = serde_json::json!({
                "schema": 1,
                "source_commit": self.manifest.source_commit,
                "fence": owned_fence_binding(&fence, self.expected_owner.0)?,
            });
            install::atomic_file(
                &self.config.transaction_dir.join("bootstrap-fence.json"),
                &serde_json::to_vec(&binding).map_err(|_| "cannot retain bootstrap fence")?,
                0o600,
                Some(self.expected_owner),
            )?;
            self.install_rust()?;
            drop(endpoint_lock);
            self.start_rust()?;
            let checks = self.verify_live()?;
            self.commit_activation(&snapshot)?;
            Ok(checks)
        })();
        match activation {
            Ok(checks) => Ok(CutoverReceipt {
                status: "bootstrapped".to_owned(),
                backup,
                database_restored: false,
                checks,
            }),
            Err(error) => {
                if self.admitted {
                    return Err(format!(
                        "bootstrap committed PostgreSQL authority; preserve it for native recovery: {error}"
                    ));
                }
                let cleanup = cleanup_bootstrap_units(
                    self.runner.as_ref(),
                    &self.config.systemctl,
                    &[&self.config.edge_unit, &self.config.daemon_unit],
                );
                if cleanup.is_err() {
                    return Err(combine(error, "stop new services", cleanup));
                }
                self.restore_installation(&snapshot)?;
                release_bootstrap_fence(
                    &self.config.transaction_dir,
                    &self.config.runtime_dir,
                    &snapshot.candidate_commit,
                    self.expected_owner.0,
                )?;
                self.finish_rollback(&snapshot)?;
                Err(format!(
                    "bootstrap failed: {error}; new installation removed; database and private backup preserved"
                ))
            }
        }
    }

    pub fn new(config: HostCutoverConfig, runner: Arc<dyn CommandRunner>) -> Result<Self, String> {
        Self::new_owned(config, runner, (0, 0))
    }

    pub fn new_owned(
        config: HostCutoverConfig,
        runner: Arc<dyn CommandRunner>,
        expected_owner: (u32, u32),
    ) -> Result<Self, String> {
        Self::new_owned_checked(config, runner, expected_owner, false)
    }

    fn new_owned_checked(
        mut config: HostCutoverConfig,
        runner: Arc<dyn CommandRunner>,
        expected_owner: (u32, u32),
        fixture_only: bool,
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
        let acceptance_sha256 = if fixture_only {
            #[cfg(feature = "selftest-fixtures")]
            {
                rehearsal::verify_fixture_inputs(&config, &manifest, expected_owner.0)?
            }
            #[cfg(not(feature = "selftest-fixtures"))]
            {
                return Err("installer rehearsal support is not compiled".into());
            }
        } else {
            install::postgres::verify_acceptance(
                &config.candidate_acceptance,
                &manifest,
                expected_owner.0,
            )?
        };
        config
            .postgres
            .verify_binaries(runner.as_ref(), expected_owner)?;
        let installed_manifest = read_json_file(&config.installed_manifest)?;
        let authority = if installed_manifest
            .as_ref()
            .and_then(|value| value.get("binaries"))
            .and_then(Value::as_array)
            .is_some_and(|binaries| {
                binaries
                    .iter()
                    .any(|binary| binary["name"] == "devcoordinator2-migrate-authority")
            }) {
            AuthorityKind::Postgres
        } else {
            AuthorityKind::LegacySqlite
        };
        let mut inherited_retirement_targets = Vec::new();
        if authority == AuthorityKind::Postgres {
            let current_path = config
                .postgres
                .data_dir
                .parent()
                .ok_or("PostgreSQL data directory has no parent")?
                .join("authority-current.json");
            let current = finalize::private(&current_path, expected_owner.0)?;
            if current["schema"] != 1
                || current["source_commit"]
                    != installed_manifest
                        .as_ref()
                        .and_then(|m| m.get("source_commit"))
                        .cloned()
                        .unwrap_or(Value::Null)
            {
                return Err(
                    "current PostgreSQL authority binding does not match the installed source"
                        .into(),
                );
            }
            let transaction = Path::new(
                current["transaction_dir"]
                    .as_str()
                    .ok_or("current PostgreSQL authority has no transaction binding")?,
            );
            if !transaction.is_absolute() {
                return Err("current PostgreSQL transaction binding is not absolute".into());
            }
            let final_path = transaction.join("finalization.json");
            let prior_finalization = if final_path.exists() {
                let receipt: FinalizationReceipt =
                    serde_json::from_value(finalize::private(&final_path, expected_owner.0)?)
                        .map_err(|_| "prior PostgreSQL finalization receipt is invalid")?;
                if receipt.schema != 1 || current["source_commit"] != receipt.source_commit {
                    return Err("prior PostgreSQL finalization belongs to another source".into());
                }
                Some(receipt)
            } else {
                None
            };
            let pending_path = transaction.join("retirement-pending.json");
            if prior_finalization
                .as_ref()
                .is_some_and(|receipt| receipt.status != "finalized")
                || (prior_finalization.is_some() && pending_path.exists())
            {
                return Err(
                    "finish the prior activation's exact finalization retry before another upgrade"
                        .into(),
                );
            }
            if pending_path.exists() {
                let pending = finalize::private(&pending_path, expected_owner.0)?;
                if pending["source_commit"] != current["source_commit"] {
                    return Err(
                        "prior PostgreSQL retirement binding belongs to another source".into(),
                    );
                }
                inherited_retirement_targets = pending["retirement_targets"]
                    .as_array()
                    .filter(|targets| targets.len() <= 32)
                    .cloned()
                    .ok_or("prior PostgreSQL activation has invalid retirement targets")?;
            } else if prior_finalization.is_none() {
                return Err(
                    "prior PostgreSQL activation has no retirement completion evidence".into(),
                );
            }
        }
        use sha2::Digest;
        let candidate = sha2::Sha256::digest(config.transaction_dir.as_os_str().as_encoded_bytes());
        config.postgres.database_name = format!(
            "devcoordinator2_candidate_{}",
            candidate[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let authority_marker_path = config
            .postgres
            .data_dir
            .parent()
            .ok_or("PostgreSQL data directory has no parent")?
            .join("authority-current.json");
        let source_root = Path::new(&manifest.source_root);
        let daemon_binary = manifest
            .binaries
            .iter()
            .find(|binary| binary.name == "devcoordinator2")
            .map(|binary| PathBuf::from(&binary.path))
            .ok_or_else(|| "candidate manifest has no control binary".to_owned())?;
        for unit in [
            &config.daemon_unit,
            &config.edge_unit,
            &config.postgres.unit,
        ] {
            if !unit.ends_with(".service")
                || unit.len() > 128
                || !unit.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@')
                })
            {
                return Err("cutover requires exact service unit names".into());
            }
        }
        let (daemon_unit_text, edge_unit_text) =
            render_host_units(&config, source_root, &daemon_binary)?;
        let tests_slice_unit_text = render_tests_slice_unit(source_root)?;
        let tmpfiles_text = install::state_tmpfiles(source_root, &config.tmpfiles_path)?;
        Ok(Self {
            authority,
            authority_marker_path,
            frozen_companions: Vec::new(),
            inherited_retirement_targets,
            config,
            runner,
            manifest,
            daemon_unit_text,
            edge_unit_text,
            tests_slice_unit_text,
            tmpfiles_text,
            expected_owner,
            fenced: false,
            admitted: false,
            acceptance_sha256,
            frozen_source_sha256: None,
            verified_backup_sha256: None,
            postgres_started: false,
            prior_edge_source: None,
            #[cfg(feature = "selftest-fixtures")]
            rehearsal_inputs: fixture_only,
            #[cfg(feature = "selftest-fixtures")]
            rehearsal_fail_edge_start: false,
        })
    }

    fn verify_acceptance_proof(&self) -> Result<String, String> {
        #[cfg(feature = "selftest-fixtures")]
        if self.rehearsal_inputs {
            return rehearsal::verify_fixture_inputs(
                &self.config,
                &self.manifest,
                self.expected_owner.0,
            );
        }
        install::postgres::verify_acceptance(
            &self.config.candidate_acceptance,
            &self.manifest,
            self.expected_owner.0,
        )
    }

    fn systemctl(&self, arguments: &[&str]) -> Result<(), String> {
        let mut arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        if self.config.ephemeral_services && arguments.first().is_some_and(|arg| arg == "enable") {
            arguments.insert(1, "--runtime".into());
        }
        let output = self.runner.run(&CommandRequest {
            program: self.config.systemctl.clone(),
            args: arguments,
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

    fn snapshot_targets(&self) -> [&Path; 12] {
        [
            &self.authority_marker_path,
            &self.config.postgres.unit_path,
            &self.config.postgres.config_path,
            &self.config.instance_env,
            &self.config.daemon_unit_path,
            &self.config.edge_unit_path,
            &self.config.edge_env,
            &self.config.tests_slice_unit_path,
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

    fn verify_candidate_binaries(&self) -> Result<(), String> {
        let verified = install::verify_manifest_binaries(&self.manifest, self.runner.as_ref())?;
        if verified != self.manifest.binaries {
            return Err(
                "candidate release binaries changed after the cutover plan was verified".to_owned(),
            );
        }
        Ok(())
    }

    fn migrate_authority(&self, mode: &str, report_name: &str) -> Result<Value, String> {
        self.migrate_at(
            mode,
            report_name,
            &self.config.transaction_dir.join("authority-before.pgdump"),
        )
    }

    fn migrate_at(&self, mode: &str, report_name: &str, archive: &Path) -> Result<Value, String> {
        self.verify_candidate_binaries()?;
        let source = self.config.transaction_dir.join("authority-before.sqlite3");
        let archive = archive.to_owned();
        let report = self.config.transaction_dir.join(report_name);
        let mut args = vec![
            "--mode".into(),
            mode.into(),
            "--source".into(),
            source.into_os_string(),
            "--database-config".into(),
            self.config.postgres.config_path.clone().into_os_string(),
            "--report".into(),
            report.clone().into_os_string(),
        ];
        if matches!(mode, "backup" | "verify-backup" | "restore-candidate") {
            args.extend(["--destination".into(), archive.into_os_string()]);
        }
        if mode == "verify-routes" {
            args.extend([
                "--state-dir".into(),
                self.config
                    .database_path
                    .parent()
                    .ok_or("authority state has no parent")?
                    .as_os_str()
                    .to_owned(),
                "--edge-state".into(),
                self.config.edge_state_dir.clone().into_os_string(),
            ]);
        }
        let output = self.runner.run(&CommandRequest {
            program: self.installed_binary("devcoordinator2-migrate-authority")?,
            args,
            environment: std::collections::BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            clear_environment: true,
        })?;
        if !output.success || output.stdout_truncated || output.stderr_truncated {
            let retain = |extension: &str, bytes: &[u8]| {
                let path = report.with_extension(extension);
                match path.symlink_metadata() {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        install::atomic_file(&path, bytes, 0o600, Some(self.expected_owner)).is_ok()
                    }
                    _ => false,
                }
            };
            let stdout_retained = retain("stdout", output.stdout.as_bytes());
            let stderr_retained = retain("stderr", output.stderr.as_bytes());
            let metadata = serde_json::to_vec(&serde_json::json!({
                "schema":1,
                "operation":mode,
                "source_commit":self.manifest.source_commit,
                "success":output.success,
                "stdout_captured_bytes":output.stdout.len(),
                "stderr_captured_bytes":output.stderr.len(),
                "stdout_truncated":output.stdout_truncated,
                "stderr_truncated":output.stderr_truncated,
                "stdout_retained":stdout_retained,
                "stderr_retained":stderr_retained
            }));
            let metadata_retained = metadata
                .as_ref()
                .is_ok_and(|bytes| retain("command.json", bytes));
            let diagnostics = if stdout_retained && stderr_retained && metadata_retained {
                "private helper diagnostics retained"
            } else {
                "private helper diagnostics incomplete"
            };
            let capture = if output.stdout_truncated || output.stderr_truncated {
                "helper output capture truncated"
            } else {
                "helper output capture complete"
            };
            return Err(format!(
                "PostgreSQL authority {mode} failed; {capture}; {diagnostics}; legacy source remains retained"
            ));
        }
        let metadata = report
            .symlink_metadata()
            .map_err(|_| "PostgreSQL migration did not retain its verification receipt")?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != self.expected_owner.0
            || metadata.mode() & 0o777 != 0o600
        {
            return Err("PostgreSQL verification receipt is not private and owned".into());
        }
        let receipt =
            read_json_file(&report)?.ok_or("PostgreSQL verification receipt is unavailable")?;
        if matches!(mode, "import" | "verify") {
            let checks = receipt["checks"]
                .as_array()
                .ok_or("PostgreSQL verification receipt has no table checks")?;
            if checks.is_empty()
                || checks.iter().any(|row| row["matched"] != true)
                || receipt["source_unchanged"] != true
                || receipt["foreign_keys_valid"] != true
                || receipt["identities_restored"] != true
                || receipt["committed"] != true
            {
                return Err(
                    "PostgreSQL authority copy did not pass every equality and integrity check"
                        .into(),
                );
            }
        } else if receipt["verified"] != true {
            return Err("PostgreSQL archive did not pass recovery verification".into());
        }
        Ok(receipt)
    }

    fn retain_prior_edge_source(&mut self) -> Result<(), String> {
        if self.prior_edge_source.is_some() {
            return Ok(());
        }
        let prior = install::read_and_verify_prior_manifest_owned(
            &self.config.installed_manifest,
            self.runner.as_ref(),
            self.expected_owner.0,
        )?;
        if prior.source_root != self.manifest.source_root {
            return Err("prior edge source belongs to another canonical installation".into());
        }
        let source = install::edge_source::prepare(
            &prior,
            &self.config.source_cache_dir,
            self.expected_owner,
            self.runner.as_ref(),
        )?;
        let binding = PriorEdgeSource {
            source: source.clone(),
            installed_manifest: self.config.installed_manifest.clone(),
            manifest_sha256: install::hash_file(&self.config.installed_manifest)?,
            edge_unit_path: self.config.edge_unit_path.clone(),
            edge_env: self.config.edge_env.clone(),
        };
        install::ensure_owned_directory(&self.config.transaction_dir, 0o700, self.expected_owner)?;
        install::atomic_file(
            &self.config.transaction_dir.join("edge-source-before.json"),
            &serde_json::to_vec_pretty(&binding).map_err(|_| "cannot retain prior edge source")?,
            0o600,
            Some(self.expected_owner),
        )?;
        self.prior_edge_source = Some(source);
        Ok(())
    }

    fn verify_critical_reads(&self) -> Result<(), String> {
        for (operation, params) in [
            ("repository.list", serde_json::json!({"limit":1})),
            ("deployment.list", serde_json::json!({})),
            ("health.summary", serde_json::json!({})),
        ] {
            let output = self.runner.run(&CommandRequest {
                program: self.installed_binary("devcoordinator2")?,
                args: vec![
                    "call".into(),
                    operation.into(),
                    "--params".into(),
                    params.to_string().into(),
                ],
                environment: std::collections::BTreeMap::from([
                    (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                    (
                        OsString::from("DEVCOORDINATOR2_INSTANCE_ENV"),
                        self.config.instance_env.as_os_str().to_owned(),
                    ),
                ]),
                clear_environment: true,
            })?;
            let value = serde_json::from_str::<Value>(&output.stdout)
                .map_err(|_| format!("candidate {operation} returned no bounded typed result"))?;
            if !output.success || output.stdout_truncated || !successful_v3_ping(&value) {
                return Err(format!(
                    "candidate {operation} failed before write admission"
                ));
            }
        }
        Ok(())
    }

    fn retain_activation_commit(&mut self, snapshot: &InstallationSnapshot) -> Result<(), String> {
        self.verify_candidate_binaries()?;
        let mut retirement_targets = self.inherited_retirement_targets.clone();
        if self.authority == AuthorityKind::LegacySqlite {
            if self.frozen_source_sha256.as_deref()
                != Some(install::hash_file(&self.config.database_path)?.as_str())
                || install::hash_file(
                    &self.config.transaction_dir.join("authority-before.sqlite3"),
                )? != snapshot.backup_sha256
            {
                return Err("frozen authority source changed before PostgreSQL admission".into());
            }
            retirement_targets.extend([serde_json::json!({"path":self.config.database_path,"sha256":self.frozen_source_sha256}),serde_json::json!({"path":self.config.transaction_dir.join("authority-before.sqlite3"),"sha256":snapshot.backup_sha256})]);
            for suffix in ["-wal", "-shm", "-journal"] {
                let path =
                    PathBuf::from(format!("{}{}", self.config.database_path.display(), suffix));
                if let Ok(metadata) = path.symlink_metadata() {
                    let Some((_, digest)) = self
                        .frozen_companions
                        .iter()
                        .find(|(frozen, _)| frozen == &path)
                    else {
                        return Err(
                            "legacy authority gained a companion after the source freeze".into(),
                        );
                    };
                    if metadata.file_type().is_symlink()
                        || !metadata.is_file()
                        || metadata.uid() != self.expected_owner.0
                        || install::hash_file(&path)? != *digest
                    {
                        return Err(
                            "frozen SQLite companion changed before native admission".into()
                        );
                    }
                    retirement_targets.push(serde_json::json!({"path":path,"sha256":digest}));
                }
            }
        }
        if self.verify_acceptance_proof()? != self.acceptance_sha256 {
            return Err("candidate acceptance changed during activation".into());
        }
        self.verify_candidate_binaries()?;
        let mut native = snapshot.clone();
        native.schema = 3;
        native.backup_sha256 =
            install::hash_file(&self.config.transaction_dir.join("authority-before.pgdump"))?;
        let activated_at_ms =
            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
        let fence = self.config.runtime_dir.join("daemon.pre-cutover.sock");
        let fence_binding = owned_fence_binding(&fence, self.expected_owner.0)?;
        let accepted = self
            .config
            .transaction_dir
            .join("candidate-acceptance.json");
        atomic_copy(
            &self.config.candidate_acceptance,
            &accepted,
            self.expected_owner,
        )?;
        if install::hash_file(&accepted)? != self.acceptance_sha256 {
            return Err("candidate acceptance changed before authority commitment".into());
        }
        let service_bindings = [
            &self.config.daemon_unit_path,
            &self.config.edge_unit_path,
            &self.config.postgres.unit_path,
        ]
        .iter()
        .map(|path| Ok(serde_json::json!({"path":path,"sha256":install::hash_file(path)?})))
        .collect::<Result<Vec<_>, String>>()?;
        let pending = serde_json::json!({"schema":1,"source_commit":self.manifest.source_commit,"activated_at_ms":activated_at_ms,"retirement_targets":retirement_targets,"postgres_config":self.config.postgres.config_path,"postgres_config_sha256":install::hash_file(&self.config.postgres.config_path)?,"database_name":self.config.postgres.database_name,"authority_marker":self.authority_marker_path,"installed_manifest":self.config.installed_manifest,"instance_env":self.config.instance_env,"instance_env_sha256":install::hash_file(&self.config.instance_env)?,"edge_env":self.config.edge_env,"edge_env_sha256":install::hash_file(&self.config.edge_env)?,"socket_path":self.config.socket_path,"postgres_unit":self.config.postgres.unit,"daemon_unit":self.config.daemon_unit,"edge_unit":self.config.edge_unit,"service_bindings":service_bindings,"candidate_acceptance":accepted,"candidate_acceptance_sha256":self.acceptance_sha256,"fence":fence_binding});
        install::atomic_file(
            &self.config.transaction_dir.join("retirement-pending.json"),
            &serde_json::to_vec(&pending)
                .map_err(|_| "cannot retain PostgreSQL activation boundary")?,
            0o600,
            Some(self.expected_owner),
        )?;
        write_snapshot(&native, "committed", self.expected_owner)?;
        // Once this record is durable, recovery must keep PostgreSQL even if
        // publishing the current-authority marker fails before fence removal.
        self.admitted = true;
        let current = serde_json::json!({"schema":1,"source_commit":self.manifest.source_commit,"transaction_dir":self.config.transaction_dir,"database_name":self.config.postgres.database_name,"activated_at_ms":activated_at_ms});
        install::atomic_file(
            &self.authority_marker_path,
            &serde_json::to_vec(&current)
                .map_err(|_| "cannot retain current PostgreSQL authority binding")?,
            0o600,
            Some(self.expected_owner),
        )?;

        Ok(())
    }

    fn verify_native_routes(&self) -> Result<(), String> {
        self.migrate_authority("verify-routes", "native-routes-verified.json")
            .map(|_| ())
    }
}

impl CutoverAdapter for HostCutover {
    type Drain = HostDrain;
    type InstallationSnapshot = InstallationSnapshot;

    fn close_admission(&mut self) -> Result<Self::Drain, String> {
        self.verify_candidate_binaries()?;
        match self.config.installed_manifest.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => self.retain_prior_edge_source()?,
        }
        begin_drain(&self.config.runtime_dir, self.expected_owner)
    }

    fn wait_for_quiescence(&mut self, _drain: &Self::Drain) -> Result<(), String> {
        wait_for_zero_activity(&self.config.runtime_dir)?;
        self.verify_candidate_binaries()
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
        if self.authority == AuthorityKind::Postgres {
            install::ensure_owned_directory(
                &self.config.transaction_dir,
                0o700,
                self.expected_owner,
            )?;
            let receipt =
                self.migrate_authority("inspect-postgres", "native-source-inspected.json")?;
            if receipt["schema_version"]
                .as_u64()
                .is_none_or(|version| version < 35 || version > u64::from(DATABASE_SCHEMA_VERSION))
            {
                return Err("native source schema is not supported by this candidate".into());
            }
            return receipt["applying"]
                .as_bool()
                .ok_or_else(|| "native activity inspection did not return a typed result".into());
        }
        let connection = open_database_read_only(&self.config.database_path)?;
        if database_schema_from(&connection)? != "34" {
            return Err("PostgreSQL cutover requires the complete frozen SQLite schema 34".into());
        }
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM deployments WHERE state='applying')",
                [],
                |row| Ok(row.get::<_, i64>(0)? != 0),
            )
            .map_err(|error| format!("cannot inspect deployment activity: {error}"))
    }

    fn backup_database(&mut self) -> Result<String, String> {
        startup_guard::require_active(
            &self.config.transaction_dir,
            &self.config.daemon_unit,
            self.runner.as_ref(),
            self.expected_owner.0,
        )?;
        startup_guard::require_active(
            &startup_guard::edge_transaction(&self.config.transaction_dir),
            &self.config.edge_unit,
            self.runner.as_ref(),
            self.expected_owner.0,
        )?;
        startup_guard::require_source_stopped(
            &startup_guard::edge_transaction(&self.config.transaction_dir),
            &self.config.systemctl,
            self.runner.as_ref(),
            &self.config.edge_unit,
            self.expected_owner.0,
        )?;
        startup_guard::require_source_stopped(
            &self.config.transaction_dir,
            &self.config.systemctl,
            self.runner.as_ref(),
            &self.config.daemon_unit,
            self.expected_owner.0,
        )?;
        install::ensure_owned_directory(&self.config.transaction_dir, 0o700, self.expected_owner)
            .map_err(|error| format!("cannot prepare cutover transaction: {error}"))?;
        if self.authority == AuthorityKind::Postgres {
            self.migrate_authority("backup", "source-native-backup.json")?;
            self.migrate_authority("verify-backup", "source-native-backup-verified.json")?;
            self.verified_backup_sha256 = Some(install::hash_file(
                &self.config.transaction_dir.join("authority-before.pgdump"),
            )?);
            return path_text(&self.config.transaction_dir.join("authority-before.pgdump"));
        }
        self.backup_legacy_database()
    }

    fn capture_installation(&mut self) -> Result<Self::InstallationSnapshot, String> {
        let mut snapshot = read_snapshot(&self.config.transaction_dir, self.expected_owner.0)?;
        startup_guard::require_freezing(&snapshot)?;
        startup_guard::unchanged(&snapshot, self.expected_owner.0)?;
        startup_guard::unchanged_effective(
            &snapshot,
            &self.config.systemctl,
            self.runner.as_ref(),
            self.expected_owner.0,
        )?;
        match self.config.installed_manifest.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => self.retain_prior_edge_source()?,
        }
        startup_guard::unchanged(&snapshot, self.expected_owner.0)?;
        startup_guard::unchanged_effective(
            &snapshot,
            &self.config.systemctl,
            self.runner.as_ref(),
            self.expected_owner.0,
        )?;
        snapshot.backup_sha256 = install::hash_file(&self.config.transaction_dir.join(
            if self.authority == AuthorityKind::Postgres {
                "authority-before.pgdump"
            } else {
                "authority-before.sqlite3"
            },
        ))?;
        if self.verified_backup_sha256.as_deref() != Some(snapshot.backup_sha256.as_str()) {
            return Err(
                "freezing snapshot has no matching verified backup; preserve the start guard"
                    .into(),
            );
        }
        snapshot.status = "prepared".into();
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
        if self.config.installed_manifest.symlink_metadata().is_ok() {
            self.retain_prior_edge_source()?;
        }
        startup_guard::capture_freezing(
            &self.config,
            &self.manifest,
            self.authority == AuthorityKind::Postgres,
            &self.snapshot_targets(),
            self.runner.as_ref(),
            self.expected_owner,
        )?;
        startup_guard::stop_services(&self.config, self.runner.as_ref(), self.expected_owner)
    }

    fn install_rust(&mut self) -> Result<(), String> {
        let checkout_commit = install::validate_live_checkout(
            Path::new(&self.manifest.source_root),
            false,
            self.runner.as_ref(),
        )?;
        if checkout_commit != self.manifest.source_commit {
            return Err("Console source no longer matches the installation candidate".into());
        }
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
        install::atomic_file(
            &self.config.tests_slice_unit_path,
            self.tests_slice_unit_text.as_bytes(),
            0o644,
            Some(self.expected_owner),
        )?;
        install::write_manifest(
            &self.config.installed_manifest,
            &self.manifest,
            self.expected_owner,
        )?;
        // The candidate paths are mutable checkout artifacts. Revalidate after
        // quiescence and immediately before exposing any managed link so a
        // build that ran while activation drained cannot silently install a
        // different source or binary hash.
        self.verify_candidate_binaries()?;
        install::configure_edge_source_revision(
            &self.config.edge_env,
            &self.manifest.source_commit,
            self.expected_owner.0,
        )?;
        install::configure_edge_console_source(
            &self.config.edge_env,
            Path::new(&self.manifest.source_root),
            self.expected_owner.0,
        )?;
        replace_captured_target(
            &self.config.cli_link,
            &self.installed_binary("devcoordinator2")?,
        )?;
        replace_captured_target(
            &self.config.tooling_link,
            &self.installed_binary("devcoordinator2-tooling")?,
        )?;
        install::set_env_value(
            &self.config.instance_env,
            "DEVCOORDINATOR2_DATABASE_CONFIG",
            &path_text(&self.config.postgres.config_path)?,
        )?;
        self.config.postgres.prepare(
            Path::new(&self.manifest.source_root),
            self.runner.as_ref(),
            self.expected_owner,
        )?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", &self.config.postgres.unit])?;
        self.systemctl(&["start", &self.config.postgres.unit])?;
        self.postgres_started = true;
        self.config.postgres.bootstrap(self.runner.as_ref())?;
        if self.authority == AuthorityKind::LegacySqlite {
            self.migrate_authority("import", "import.json")?;
            self.systemctl(&["restart", &self.config.postgres.unit])?;
            self.config.postgres.wait_ready(self.runner.as_ref())?;
            self.migrate_authority("verify", "restart-verified.json")?;
            self.migrate_authority("backup", "postgres-backup.json")?;
            self.migrate_authority("verify-backup", "postgres-backup-verified.json")?;
        } else {
            self.migrate_authority("restore-candidate", "native-candidate-restored.json")?;
            self.migrate_authority("verify-backup", "native-candidate-copy-verified.json")?;
            self.migrate_authority("migrate", "native-candidate-migrated.json")?;
            self.systemctl(&["restart", &self.config.postgres.unit])?;
            self.config.postgres.wait_ready(self.runner.as_ref())?;
            let archive = self
                .config
                .transaction_dir
                .join("authority-candidate.pgdump");
            self.migrate_at("backup", "native-candidate-backup.json", &archive)?;
            self.migrate_at(
                "verify-backup",
                "native-candidate-restart-verified.json",
                &archive,
            )?;
        }
        Ok(())
    }

    fn start_rust(&mut self) -> Result<(), String> {
        startup_guard::release_services(
            &self.config.transaction_dir,
            &self.config.daemon_unit,
            &self.config.edge_unit,
            &self.config.systemctl,
            self.runner.as_ref(),
            self.expected_owner,
        )?;
        #[cfg(feature = "selftest-fixtures")]
        if self.rehearsal_fail_edge_start {
            // Only the marker-validated fixture constructor can arm this. A
            // real Node startup fails; no command or success response is mocked.
            install::set_env_value(&self.config.edge_env, "EDGE_BASE_DOMAIN", "")?;
        }
        self.systemctl_all(&[
            ("enable", &self.config.daemon_unit),
            ("restart", &self.config.daemon_unit),
            ("enable", &self.config.edge_unit),
            ("restart", &self.config.edge_unit),
        ])
    }

    fn verify_live(&mut self) -> Result<Vec<String>, String> {
        let binary = self.installed_binary("devcoordinator2")?;
        // A cold daemon may need to open and integrity-check a large authority
        // database before it can answer the first ping. Keep the readiness
        // bound finite, but do not turn that legitimate startup work into a
        // rollback on hosts with multi-gigabyte state.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let output = self.runner.run(&CommandRequest {
                program: binary.clone(),
                args: vec!["call".into(), "ping".into()],
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
            if output.success
                && response.as_ref().is_some_and(|response| {
                    successful_v3_ping(response)
                        && response["data"]["source_commit"].as_str()
                            == Some(self.manifest.source_commit.as_str())
                        && response["data"]["socket"].as_str() == self.config.socket_path.to_str()
                })
            {
                break;
            }
            if !response.as_ref().is_some_and(retryable_v3_ping)
                || std::time::Instant::now() >= deadline
            {
                return Err(command_failure("v3 ping", &output.stderr, &output.stdout));
            }
            self.systemctl(&["is-active", "--quiet", &self.config.daemon_unit])?;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.systemctl(&["is-active", "--quiet", &self.config.daemon_unit])?;
        self.systemctl(&["is-active", "--quiet", &self.config.edge_unit])?;
        self.verify_native_routes()?;
        self.verify_critical_reads()?;
        self.systemctl(&["is-active", "--quiet", &self.config.daemon_unit])?;
        self.systemctl(&["is-active", "--quiet", &self.config.edge_unit])?;
        Ok(vec![
            "route-lease-edge-reconciled".to_owned(),
            "v3-cli-ping".to_owned(),
            "rust-daemon-active".to_owned(),
            "node-edge-active".to_owned(),
            if self.authority == AuthorityKind::LegacySqlite {
                "postgres-import-equal-after-restart"
            } else {
                "native-authority-upgrade-verified"
            }
            .to_owned(),
            "postgres-backup-restored-and-verified".to_owned(),
            "frozen-candidate-acceptance".to_owned(),
            "critical-read-api".to_owned(),
        ])
    }

    fn commit_activation(&mut self, snapshot: &Self::InstallationSnapshot) -> Result<(), String> {
        self.retain_activation_commit(snapshot)?;
        // The durable authority choice is irreversible even if the final
        // fence unlink fails. Recovery verifies and finishes this same commit.
        self.admitted = true;
        let pending = finalize::private(
            &self.config.transaction_dir.join("retirement-pending.json"),
            self.expected_owner.0,
        )?;
        release_committed_fence(&self.config.runtime_dir, &pending, self.expected_owner.0)?;
        self.fenced = false;
        self.admitted = true;
        Ok(())
    }

    fn writes_admitted(&self) -> bool {
        self.admitted
    }

    fn finalization_required(&self) -> bool {
        true
    }

    fn stop_rust(&mut self) -> Result<(), String> {
        startup_guard::stop_services(&self.config, self.runner.as_ref(), self.expected_owner)?;
        if self.postgres_started {
            self.systemctl(&["stop", &self.config.postgres.unit])?;
            self.postgres_started = false;
        }
        Ok(())
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
        if self.authority == AuthorityKind::Postgres {
            self.systemctl(&["start", &self.config.postgres.unit])?;
            self.config.postgres.wait_ready(self.runner.as_ref())?;
            self.migrate_authority("verify-backup", "native-source-rollback-verified.json")?;
            return Ok(true);
        }

        if !database_integrity(&self.config.database_path)? {
            return Ok(false);
        }
        if !database_integrity(Path::new(backup))? {
            return Err("cutover database backup failed integrity verification".to_owned());
        }
        Ok(database_schema(&self.config.database_path)? == database_schema(Path::new(backup))?)
    }

    fn restore_database(&mut self, backup: &str) -> Result<(), String> {
        if self.authority == AuthorityKind::Postgres {
            return Err("native source database is preserved; restore its captured configuration rather than overwriting PostgreSQL data".into());
        }

        if self.admitted {
            return Err("cannot restore SQLite after PostgreSQL writes were admitted".into());
        }
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
        if let Ok(snapshot) = read_snapshot(&self.config.transaction_dir, self.expected_owner.0)
            && snapshot.status == "freezing"
        {
            let config = RecoveryConfig {
                transaction_dir: self.config.transaction_dir.clone(),
                runtime_dir: self.config.runtime_dir.clone(),
                socket_path: self.config.socket_path.clone(),
                database_path: self.config.database_path.clone(),
                systemctl: self.config.systemctl.clone(),
                daemon_unit: self.config.daemon_unit.clone(),
                edge_unit: self.config.edge_unit.clone(),
                edge_state_dir: self.config.edge_state_dir.clone(),
                source_root: self.manifest.source_root.clone().into(),
                edge_env: self.config.edge_env.clone(),
            };
            startup_guard::resume_freezing(
                &config,
                &snapshot,
                self.runner.as_ref(),
                self.expected_owner,
            )?;
            return Ok(());
        }
        restore_prior_edge_source(
            &self.config.transaction_dir,
            Path::new(&self.manifest.source_root),
            &self.config.edge_env,
            None,
            self.runner.as_ref(),
            self.expected_owner,
        )?;
        self.systemctl(&["daemon-reload"])?;
        startup_guard::release_services(
            &self.config.transaction_dir,
            &self.config.daemon_unit,
            &self.config.edge_unit,
            &self.config.systemctl,
            self.runner.as_ref(),
            self.expected_owner,
        )?;
        self.systemctl_all(&[
            ("restart", &self.config.daemon_unit),
            ("restart", &self.config.edge_unit),
        ])
    }

    fn reopen_admission(&mut self, drain: Self::Drain) -> Result<(), String> {
        end_drain(&drain)
    }
}

fn restore_prior_edge_source(
    transaction: &Path,
    source_root: &Path,
    edge_env: &Path,
    snapshot: Option<&InstallationSnapshot>,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    let binding =
        read_prior_edge_source(transaction, source_root, edge_env, snapshot, runner, owner)?;
    install::edge_source::bind(
        &binding.source,
        &binding.edge_unit_path,
        edge_env,
        "devcoordinator2-edge",
        owner.0,
        runner,
    )
}

/// Validate the retained binding without changing source, units or environment.
fn read_prior_edge_source(
    transaction: &Path,
    source_root: &Path,
    edge_env: &Path,
    snapshot: Option<&InstallationSnapshot>,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<PriorEdgeSource, String> {
    let binding: PriorEdgeSource = serde_json::from_value(finalize::private(
        &transaction.join("edge-source-before.json"),
        owner.0,
    )?)
    .map_err(|_| "prior edge source binding is invalid")?;
    if binding.source.repository != source_root || binding.edge_env != edge_env {
        return Err("prior edge source belongs to another installation".into());
    }
    if let Some(snapshot) = snapshot {
        for path in [
            &binding.installed_manifest,
            &binding.edge_unit_path,
            &binding.edge_env,
        ] {
            if !snapshot
                .entries
                .iter()
                .any(|entry| Path::new(&entry.path) == path)
            {
                return Err("prior edge source is outside the captured installation".into());
            }
        }
    }
    let manifest = install::read_and_verify_prior_manifest_owned(
        &binding.installed_manifest,
        runner,
        owner.0,
    )?;
    if manifest.source_commit != binding.source.source_commit
        || install::hash_file(&binding.installed_manifest)? != binding.manifest_sha256
    {
        return Err("prior edge manifest no longer matches its captured source".into());
    }
    Ok(binding)
}

fn render_host_units(
    config: &HostCutoverConfig,
    source: &Path,
    daemon: &Path,
) -> Result<(String, String), String> {
    let path = |path: &Path| -> Result<String, String> {
        let value = path_text(path)?;
        if !path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || value.bytes().any(|byte| {
                byte.is_ascii_whitespace() || matches!(byte, b'%' | b'\\' | b'\'' | b'"')
            })
        {
            return Err("service bindings require exact absolute unit paths".into());
        }
        Ok(value)
    };
    let instance = path(&config.instance_env)?;
    let state = path(
        config
            .database_path
            .parent()
            .ok_or("authority state directory is unavailable")?,
    )?;
    let runtime = path(&config.runtime_dir)?;
    let etc = path(
        config
            .instance_env
            .parent()
            .ok_or("instance environment has no parent")?,
    )?;
    let daemon=render_daemon_unit_for_binary(source,daemon)?
        .replace("devcoordinator2-postgres.service",&config.postgres.unit)
        .replace("EnvironmentFile=-/etc/devcoordinator2/instance.env",&format!("EnvironmentFile=-{instance}\nEnvironment=DEVCOORDINATOR2_INSTANCE_ENV={instance}"))
        .replace("ReadWritePaths=/home /var/lib/devcoordinator2 /run/devcoordinator2 /etc/devcoordinator2",&format!("ReadWritePaths=/home {state} {runtime} {etc}"));
    let edge_env = path(&config.edge_env)?;
    let edge_state = path(&config.edge_state_dir)?;
    let credentials = config
        .edge_env
        .parent()
        .ok_or("edge environment has no parent")?
        .join("edge");
    let credential = path(&credentials)?;
    let mut edge = render_edge_unit(source, config.canary)?
        .replace(
            "EnvironmentFile=/etc/devcoordinator2/edge.env",
            &format!("EnvironmentFile={edge_env}"),
        )
        .replace("/etc/devcoordinator2/edge/", &format!("{credential}/"))
        .replace(
            "Environment=EDGE_STATE_DIR=/var/lib/devcoordinator2-edge",
            &format!("Environment=EDGE_STATE_DIR={edge_state}"),
        )
        .replace(
            "ReadOnlyPaths=-/var/lib/devcoordinator2/public",
            &format!("ReadOnlyPaths=-{state}/public"),
        );
    if config.edge_state_dir != Path::new("/var/lib/devcoordinator2-edge") {
        edge = edge.replace(
            "StateDirectory=devcoordinator2-edge\n",
            &format!("ReadWritePaths={edge_state}\n"),
        );
    }
    if !daemon.contains(&format!("EnvironmentFile=-{instance}\n"))
        || !daemon.contains(&format!(
            "Environment=DEVCOORDINATOR2_INSTANCE_ENV={instance}\n"
        ))
        || !edge.contains(&format!("EnvironmentFile={edge_env}\n"))
        || !edge.contains(&format!("Environment=EDGE_STATE_DIR={edge_state}\n"))
        || !edge.contains(&format!(
            "LoadCredential=session.secret:{credential}/session.secret\n"
        ))
    {
        return Err("source units do not bind the configured instance and credential paths".into());
    }
    let daemon = if config.ephemeral_services {
        #[cfg(not(feature = "selftest-fixtures"))]
        return Err("ephemeral service profiles require the installer rehearsal feature".into());
        #[cfg(feature = "selftest-fixtures")]
        {
            let root = path(&rehearsal::service_root(config)?)?;
            edge = edge.replace(
                "[Service]\n",
                &format!("[Service]\nPrivateNetwork=yes\nTimeoutStopSec=15\nStandardOutput=append:{root}/service-logs/edge.log\nStandardError=append:{root}/service-logs/edge.log\n"),
            );
            let daemon = daemon
                .replace("ProtectSystem=full\n", "ProtectSystem=strict\n")
                .replace(
                    &format!("ReadWritePaths=/home {state} {runtime} {etc}\n"),
                    &format!("ReadWritePaths={root}\n"),
                )
                .replace("[Service]\n", &format!("[Service]\nPrivateTmp=yes\nTimeoutStopSec=15\nStandardOutput=append:{root}/service-logs/daemon.log\nStandardError=append:{root}/service-logs/daemon.log\n"));
            let writable = format!("ReadWritePaths={root}");
            let mut writable_directives = daemon
                .lines()
                .filter(|line| line.starts_with("ReadWritePaths="));
            if !daemon.lines().any(|line| line == "ProtectSystem=strict")
                || writable_directives.next() != Some(writable.as_str())
                || writable_directives.next().is_some()
            {
                return Err("fixture daemon does not isolate writes to its owned root".into());
            }
            daemon
        }
    } else {
        daemon
    };
    Ok((daemon, edge))
}

fn successful_v3_ping(response: &Value) -> bool {
    response.get("protocol").and_then(Value::as_u64) == Some(3)
        && response.get("ok") == Some(&Value::Bool(true))
}

fn owned_fence_binding(path: &Path, owner: u32) -> Result<Value, String> {
    let metadata = path.symlink_metadata().map_err(
        |_| "the exact committed activation fence is unavailable; preserve PostgreSQL authority",
    )?;
    if metadata.file_type().is_symlink()
        || !(metadata.file_type().is_socket() || cfg!(test) && metadata.is_file())
        || metadata.uid() != owner
    {
        return Err("committed activation fence is not owned by this installation".into());
    }
    Ok(
        serde_json::json!({"path":path,"identity":file_identity(&metadata),"uid":metadata.uid(),"gid":metadata.gid(),"mode":metadata.mode()}),
    )
}

fn release_committed_fence(runtime: &Path, pending: &Value, owner: u32) -> Result<(), String> {
    let fence = runtime.join("daemon.pre-cutover.sock");
    if owned_fence_binding(&fence, owner)? != pending["fence"] {
        return Err("committed activation fence changed; PostgreSQL remains authoritative".into());
    }
    std::fs::remove_file(&fence).map_err(|_| "cannot release exact committed activation fence")?;
    File::open(runtime)
        .and_then(|file| file.sync_all())
        .map_err(|_| "cannot seal committed activation admission")?;
    Ok(())
}

fn release_bootstrap_fence(
    transaction: &Path,
    runtime: &Path,
    candidate: &str,
    owner: u32,
) -> Result<(), String> {
    let record = transaction.join("bootstrap-fence.json");
    match record.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match runtime.join("daemon.pre-cutover.sock").symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err("unrecorded bootstrap fence must be preserved".into()),
            };
        }
        Err(_) => return Err("cannot inspect bootstrap fence record".into()),
        Ok(_) => {}
    }
    let binding = finalize::private(&record, owner)?;
    if binding["schema"] != 1 || binding["source_commit"] != candidate {
        return Err("bootstrap fence belongs to another candidate".into());
    }
    release_committed_fence(runtime, &binding, owner)
}

fn retryable_v3_ping(response: &Value) -> bool {
    response.get("protocol").and_then(Value::as_u64) == Some(3)
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
    if snapshot.status == "freezing" {
        startup_guard::require_freezing(&snapshot)?;
        startup_guard::validate_freezing_source(config, &snapshot, runner, expected_owner)?;
        let drain = begin_drain(&config.runtime_dir, expected_owner)?;
        let receipt = startup_guard::resume_freezing(config, &snapshot, runner, expected_owner)?;
        end_drain(&drain)?;
        return Ok(receipt);
    }
    if snapshot.schema == 3 && snapshot.status == "committed" {
        return native_recovery::finish_activation(config, &snapshot, runner, expected_owner);
    }
    if snapshot.schema == 3
        && !matches!(
            snapshot.status.as_str(),
            "prepared" | "activating" | "rolling_back"
        )
    {
        return Err("PostgreSQL authority has crossed admission; recovery must preserve PostgreSQL writes and roll forward".into());
    }
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
        let absent_bootstrap = snapshot.bootstrap
            && matches!(entry.kind, SnapshotKind::Missing)
            && matches!(std::fs::symlink_metadata(&entry.path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
        if !prior_matches
            && !absent_bootstrap
            && !current.as_ref().is_some_and(|doc| {
                doc["source_commit"].as_str() == Some(snapshot.candidate_commit.as_str())
            })
        {
            return Err("recovery target belongs to a different installed candidate".into());
        }
    }
    if snapshot.schema == 3 {
        return native_recovery::recover(config, &snapshot, runner, expected_owner);
    }
    let backup_path = config.transaction_dir.join("authority-before.sqlite3");
    if snapshot.bootstrap && snapshot.backup_sha256.is_empty() {
        if !matches!(backup_path.symlink_metadata(), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err("unexpected backup for database-free bootstrap".into());
        }
    } else {
        if install::hash_file(&backup_path).map_err(invalid_target)? != snapshot.backup_sha256 {
            return Err("recovery target backup does not match its transaction".into());
        }
        if !database_integrity(&backup_path).map_err(invalid_target)? {
            return Err("recovery target backup failed integrity verification".into());
        }
    }
    if snapshot.bootstrap {
        let bootstrap_drain = begin_drain(&config.runtime_dir, expected_owner)?;
        cleanup_bootstrap_units(
            runner,
            &config.systemctl,
            &[&config.edge_unit, &config.daemon_unit],
        )?;
        for entry in snapshot.entries.iter().rev() {
            restore_entry(entry)?;
        }
        run_systemctl(runner, &config.systemctl, &["daemon-reload"])?;
        release_bootstrap_fence(
            &config.transaction_dir,
            &config.runtime_dir,
            &snapshot.candidate_commit,
            expected_owner.0,
        )?;
        end_drain(&bootstrap_drain)?;
        write_snapshot(&snapshot, "recovered", expected_owner)?;
        return Ok(RecoveryReceipt {
            status: "recovered".to_owned(),
            database_restored: false,
            snapshot: path_text(&config.transaction_dir.join("installation-snapshot.json"))?,
        });
    }
    // Reject a source that is already known to be unverified without taking
    // down the serving edge or changing its private settings. The later
    // refresh remains inside the recovery drain to catch concurrent drift.
    install::validate_live_checkout(&config.source_root, false, runner).map_err(|error| {
        format!(
            "Console source binding is unverified; recovery left running services and installation unchanged: {error}"
        )
    })?;
    // Share activation's atomic admission boundary with edge maintenance, and
    // keep the recovery marker present until source and route checks finish.
    let recovery_drain = begin_drain(&config.runtime_dir, expected_owner)?;
    startup_guard::apply_recovery(config, &snapshot, runner, expected_owner)?;
    run_systemctl_all(
        runner,
        &config.systemctl,
        &[("stop", &config.daemon_unit), ("stop", &config.edge_unit)],
    )?;
    startup_guard::require_stopped(&config.systemctl, runner, &config.daemon_unit)?;
    startup_guard::require_stopped(&config.systemctl, runner, &config.edge_unit)?;
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
    restore_prior_edge_source(
        &config.transaction_dir,
        &config.source_root,
        &config.edge_env,
        Some(&snapshot),
        runner,
        expected_owner,
    )?;
    run_systemctl(runner, &config.systemctl, &["daemon-reload"])?;
    restore_socket(&config.runtime_dir, &config.socket_path)?;
    startup_guard::release_services(
        &config.transaction_dir,
        &config.daemon_unit,
        &config.edge_unit,
        &config.systemctl,
        runner,
        expected_owner,
    )?;
    run_systemctl_all(
        runner,
        &config.systemctl,
        &[
            ("restart", &config.daemon_unit),
            ("restart", &config.edge_unit),
        ],
    )?;
    verify_reconciled_routes(&config.database_path, &config.edge_state_dir)?;
    end_drain(&recovery_drain)?;
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
    if !matches!(snapshot.schema, 2 | 3) || Path::new(&snapshot.transaction_dir) != transaction_dir
    {
        return Err("installation snapshot identity does not match its transaction".to_owned());
    }
    Ok(snapshot)
}

fn cleanup_bootstrap_units(
    runner: &dyn CommandRunner,
    systemctl: &Path,
    units: &[&str],
) -> Result<(), String> {
    for action in ["stop", "disable"] {
        for unit in units {
            let state = runner.run(&CommandRequest {
                program: systemctl.to_owned(),
                args: ["show", "--property=LoadState", "--value", unit]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                environment: Default::default(),
                clear_environment: true,
            })?;
            if !state.stdout_truncated && state.stdout.trim() == "not-found" {
                continue;
            }
            if !state.success || state.stdout_truncated || state.stdout.trim().is_empty() {
                return Err(format!("cannot verify bootstrap unit state: {unit}"));
            }
            run_systemctl(runner, systemctl, &[action, unit])?;
        }
    }
    Ok(())
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

/// TLS maintenance shares the installer admission boundary. The returned file
/// holds the existing lock until replacement, verification, and any rollback
/// finish; a drain marker keeps maintenance refused throughout cutover/recovery.
pub(crate) fn edge_maintenance_guard(runtime_dir: &Path) -> Result<File, String> {
    std::fs::create_dir_all(runtime_dir)
        .map_err(|_| "cannot access the existing installation admission boundary")?;
    let lock = unix_fs::open(
        runtime_dir.join(LOCK_FILE),
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o666),
    )
    .map(File::from)
    .map_err(|_| "cannot open the existing installation admission boundary")?;
    unix_fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "TLS maintenance is deferred while installation admission is busy; retry the renewal deploy hook after it completes")?;
    // Refuse even an unfinished stale drain. Only the reviewed installation or
    // recovery path may retire that lease; renewal never deletes its evidence.
    match runtime_dir.join(DRAIN_FILE).symlink_metadata() {
        Ok(_) => Err("TLS maintenance is deferred while installation or recovery is in progress; retry the renewal deploy hook after recovery completes".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(lock),
        Err(_) => Err("cannot inspect the existing installation admission boundary".into()),
    }
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
    // Deferred waits and health snapshots only observe state. A health snapshot
    // stranded by ENOSPC cannot mutate authority during the cutover backup.
    let Ok(request) = serde_json::from_value::<devcoordinator2_api::RequestEnvelope>(value.clone())
    else {
        return false;
    };
    // Protocol 2 is inspected only while draining the old source at its
    // one-time cutover; the candidate daemon itself accepts protocol 3 only.
    matches!(request.protocol, 2 | 3)
        && !request.id.is_empty()
        && request.id.len() <= 64
        && request.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        && (devcoordinator2_api::is_deferred_wait(&request.operation)
            || request.operation == "health.summary")
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
    let mut input = unix_fs::open(
        source,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| "cannot open frozen database backup")?;
    let before = input
        .metadata()
        .map_err(|_| "cannot inspect frozen database backup")?;
    if !before.is_file() {
        return Err("frozen database backup is not a regular file".into());
    }
    let parent = destination
        .parent()
        .ok_or("database restore destination has no parent")?;
    let name = destination
        .file_name()
        .ok_or("database restore destination has no leaf")?;
    let directory = unix_fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| "cannot open database restore directory")?;
    let temporary = OsString::from(format!(".authority-restore-{}", random_hex(8)?));
    let result = (|| {
        let mut output = unix_fs::openat(
            &directory,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map(File::from)
        .map_err(|_| "cannot create private database restore temporary")?;
        if unsafe { libc::fchown(output.as_raw_fd(), owner.0, owner.1) } != 0 {
            return Err("cannot retain database restore ownership".to_owned());
        }
        let copied =
            std::io::copy(&mut input, &mut output).map_err(|_| "database backup copy failed")?;
        let after = input
            .metadata()
            .map_err(|_| "cannot recheck database backup")?;
        if copied != before.len() || file_identity(&before) != file_identity(&after) {
            return Err("database backup changed during restore".into());
        }
        output
            .sync_all()
            .map_err(|_| "cannot seal database restore temporary")?;
        unix_fs::renameat(&directory, &temporary, &directory, name)
            .map_err(|_| "cannot atomically restore database backup")?;
        directory
            .sync_all()
            .map_err(|_| "cannot seal database restore directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unix_fs::unlinkat(&directory, &temporary, unix_fs::AtFlags::empty());
    }
    result
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
    if !(MINIMUM_SUPPORTED_DATABASE_SCHEMA..=MAXIMUM_LEGACY_SQLITE_SCHEMA)
        .any(|supported| schema == supported.to_string())
    {
        return Err(format!(
            "legacy recovery requires SQLite schema {MINIMUM_SUPPORTED_DATABASE_SCHEMA} through {MAXIMUM_LEGACY_SQLITE_SCHEMA}, found {schema}"
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
            (MINIMUM_SUPPORTED_DATABASE_SCHEMA..=MAXIMUM_LEGACY_SQLITE_SCHEMA).contains(&version)
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
        let mut native = health.clone();
        native["protocol"] = serde_json::json!(3);
        assert!(bridge_observation(&native));
        native["protocol"] = serde_json::json!(4);
        assert!(!bridge_observation(&native));
        assert!(bridge_observation(&request(
            "event.wait",
            serde_json::json!({
                "filters":[{"filter_id":"upgrade","categories":["health"]}]
            })
        )));
        for (operation, params) in [
            (
                "test.admission.wait",
                serde_json::json!({"deadline_at":"2100-01-01T00:00:00Z"}),
            ),
            (
                "test.wait",
                serde_json::json!({"path":"/fixture","run_id":"fixture-run","deadline_at":"2100-01-01T00:00:00Z"}),
            ),
            (
                "deployment.wait",
                serde_json::json!({"deployment_id":"fixture-deployment","state":"ready","deadline_at":"2100-01-01T00:00:00Z"}),
            ),
        ] {
            assert!(
                bridge_observation(&request(operation, params)),
                "{operation}"
            );
            assert!(
                !bridge_observation(&request(operation, serde_json::json!({"unexpected":true}))),
                "invalid {operation} cannot bypass drain"
            );
        }
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
                "stop_legacy",
                "backup_database",
                "capture_installation",
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
        fragment: PathBuf,
        commit: String,
        checkout_commit: Option<String>,
        upstream_commit: Option<String>,
        dirty_checkout: bool,
        dirty_checkout_after_stop: bool,
        fail_ping: bool,
        fail_reload: AtomicUsize,
        fail_stop: bool,
        loaded_units: bool,
        backup_verification_failure: Option<crate::install::CommandOutput>,
        no_prior: bool,
        retryable_ping_failures: AtomicUsize,
        requests: Mutex<Vec<CommandRequest>>,
    }

    impl CommandRunner for HostFake {
        fn run(&self, request: &CommandRequest) -> Result<crate::install::CommandOutput, String> {
            self.requests.lock().unwrap().push(request.clone());
            if request.program == Path::new("/usr/bin/busctl") {
                if self.no_prior
                    && request.args.iter().any(|a| a == "LoadUnit")
                    && request
                        .args
                        .last()
                        .is_some_and(|a| !self.fragment.with_file_name(a).exists())
                {
                    return Err("unconfigured unit cannot be loaded".into());
                }
                let data = if request.args.iter().any(|a| a == "LoadUnit") {
                    serde_json::json!([format!(
                        "/org/freedesktop/systemd1/unit/{}",
                        request.args.last().unwrap().to_string_lossy()
                    )])
                } else {
                    let unit = Path::new(&request.args[3]).file_name().unwrap();
                    let fragment = self.fragment.with_file_name(unit);
                    let directory = self
                        .fragment
                        .with_file_name(format!("{}.d", unit.to_string_lossy()));
                    let paths = std::fs::read_dir(directory)
                        .into_iter()
                        .flatten()
                        .map(|p| p.unwrap().path())
                        .collect::<Vec<_>>();
                    match request.args.last().and_then(|a| a.to_str()).unwrap() {
                        "FragmentPath" => {
                            serde_json::json!(fragment)
                        }
                        "LoadState" => serde_json::json!("loaded"),
                        "DropInPaths" => serde_json::json!(paths),
                        "Conditions" => serde_json::json!(
                            paths
                                .iter()
                                .flat_map(|p| {
                                    std::fs::read_to_string(p)
                                        .unwrap_or_default()
                                        .lines()
                                        .filter_map(|line| {
                                            line.strip_prefix("ConditionPathExists=!")
                                        })
                                        .map(|active| {
                                            serde_json::json!([
                                                "ConditionPathExists",
                                                false,
                                                true,
                                                active,
                                                0
                                            ])
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .collect::<Vec<_>>()
                        ),
                        _ => return Err("unexpected systemd fixture property".into()),
                    }
                };
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: serde_json::json!({"data":data}).to_string(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program.file_name() == Some(std::ffi::OsStr::new("systemctl"))
                && request.args.first().is_some_and(|a| a == "show")
            {
                if request.args.iter().any(|a| a == "--value") {
                    let loaded = self.loaded_units
                        || request
                            .args
                            .last()
                            .is_some_and(|unit| self.fragment.with_file_name(unit).is_file());
                    return Ok(crate::install::CommandOutput {
                        success: true,
                        stdout: if loaded { "loaded\n" } else { "not-found\n" }.into(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    });
                }
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: if self.no_prior
                        && request
                            .args
                            .last()
                            .is_some_and(|a| !self.fragment.with_file_name(a).exists())
                    {
                        "LoadState=not-found\nActiveState=inactive\nMainPID=0\n".into()
                    } else {
                        "LoadState=loaded\nActiveState=inactive\nMainPID=0\n".into()
                    },
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program == Path::new("/usr/bin/git") && request.args.len() >= 5 {
                let args = &request.args[4..];
                let git_root = Path::new(&request.args[3]);
                let pinned = git_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| {
                        matches!(name.len(), 40 | 64)
                            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                    });
                let stdout =
                    if args == ["rev-parse", "--show-toplevel"] {
                        request.args[3].to_string_lossy().into_owned()
                    } else if args == ["branch", "--show-current"] {
                        if pinned.is_some() {
                            String::new()
                        } else {
                            "main".to_owned()
                        }
                    } else if args == ["status", "--porcelain", "--untracked-files=all"] {
                        let stopped =
                            self.requests.lock().unwrap().iter().any(|request| {
                                request.args.first() == Some(&OsString::from("stop"))
                            });
                        if pinned.is_none()
                            && (self.dirty_checkout || (self.dirty_checkout_after_stop && stopped))
                        {
                            " M console/workspace.js\n".to_owned()
                        } else {
                            String::new()
                        }
                    } else if args == ["rev-parse", "HEAD"] {
                        pinned.map(str::to_owned).unwrap_or_else(|| {
                            self.checkout_commit
                                .as_ref()
                                .unwrap_or(&self.commit)
                                .clone()
                        })
                    } else if args == ["rev-parse", "--path-format=absolute", "--git-common-dir"] {
                        let root = if pinned.is_some() {
                            git_root.parent().unwrap().parent().unwrap().join("source")
                        } else {
                            git_root.to_owned()
                        };
                        root.join(".git").to_string_lossy().into_owned()
                    } else if args.first().is_some_and(|arg| arg == "worktree") {
                        assert_eq!(
                            &args[..4],
                            ["worktree", "add", "--detach", "--"].map(OsString::from)
                        );
                        let checkout = Path::new(&args[4]);
                        std::fs::create_dir_all(checkout.join("edge")).unwrap();
                        std::fs::create_dir_all(checkout.join("console")).unwrap();
                        std::fs::write(checkout.join(".git"), "fixture pointer").unwrap();
                        std::fs::write(
                            checkout.join("edge/devcoordinator2-edge.mjs"),
                            "fixture edge source\n",
                        )
                        .unwrap();
                        std::fs::write(
                            checkout.join("console/index.html"),
                            "fixture Console source\n",
                        )
                        .unwrap();
                        String::new()
                    } else if args.first().is_some_and(|arg| arg == "ls-tree") {
                        let digest = |bytes: &[u8]| {
                            Sha256::digest(bytes)
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        };
                        format!(
                            "100644 blob {}\tedge/devcoordinator2-edge.mjs\0\
                            100644 blob {}\tconsole/index.html\0",
                            digest(b"fixture edge source\n"),
                            digest(b"fixture Console source\n")
                        )
                    } else if args.first().is_some_and(|arg| arg == "hash-object") {
                        args.iter()
                            .skip(3)
                            .map(|path| install::hash_file(&git_root.join(path)).unwrap())
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else if args == ["rev-parse", "refs/remotes/origin/main"] {
                        self.upstream_commit
                            .as_ref()
                            .or(self.checkout_commit.as_ref())
                            .unwrap_or(&self.commit)
                            .clone()
                    } else {
                        return Err("unexpected Git checkout fixture command".into());
                    };
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout,
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if (request.args == [OsString::from("daemon-reload")]
                && self
                    .fail_reload
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok())
                || (request.args.first() == Some(&OsString::from("stop")) && self.fail_stop)
            {
                return Ok(crate::install::CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "injected systemctl failure".to_owned(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program == Path::new("/usr/bin/setpriv")
                && request
                    .args
                    .iter()
                    .any(|arg| arg.to_string_lossy().ends_with("/initdb"))
            {
                let directory = PathBuf::from(
                    &request
                        .args
                        .windows(2)
                        .find(|pair| pair[0] == "--pgdata")
                        .unwrap()[1],
                );
                std::fs::write(directory.join("PG_VERSION"), "17\n").unwrap();
            }
            if request.program == Path::new("/usr/bin/id") {
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: if request.args.first().is_some_and(|v| v == "-g") {
                        rustix::process::getgid().as_raw().to_string()
                    } else {
                        rustix::process::getuid().as_raw().to_string()
                    },
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.args == [OsString::from("--version")]
                && request
                    .program
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        [
                            "postgres",
                            "initdb",
                            "psql",
                            "pg_dump",
                            "pg_restore",
                            "pg_isready",
                        ]
                        .contains(&name)
                    })
            {
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: format!(
                        "{} (PostgreSQL) 17.6\n",
                        request.program.file_name().unwrap().to_string_lossy()
                    ),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program.file_name()
                == Some(std::ffi::OsStr::new("devcoordinator2-migrate-authority"))
                && request.args.first().is_some_and(|v| v == "--mode")
            {
                let argument = |name: &str| {
                    request
                        .args
                        .windows(2)
                        .find(|pair| pair[0] == name)
                        .map(|pair| PathBuf::from(&pair[1]))
                        .unwrap()
                };
                let mode = request.args[1].to_string_lossy();
                if mode == "verify-backup"
                    && let Some(output) = &self.backup_verification_failure
                {
                    return Ok(output.clone());
                }
                if mode == "backup" {
                    std::fs::write(
                        argument("--destination"),
                        b"PGDMP fixture native authority archive",
                    )
                    .unwrap();
                }
                let receipt = if mode == "inspect-postgres" {
                    serde_json::json!({"verified":true,"applying":false,"schema_version":35})
                } else if matches!(mode.as_ref(), "import" | "verify") {
                    serde_json::json!({"checks":[{"matched":true}],"source_unchanged":true,"foreign_keys_valid":true,"identities_restored":true,"committed":true})
                } else if matches!(
                    mode.as_ref(),
                    "backup" | "verify-backup" | "verify-recoverable-backup"
                ) {
                    serde_json::json!({"verified":true,"archive_sha256":install::hash_file(&argument("--destination")).unwrap(),"isolated_restore_removed":true})
                } else {
                    serde_json::json!({"verified":true})
                };
                let path = argument("--report");
                std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: "private verification receipt retained".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program.file_name() == Some(std::ffi::OsStr::new("devcoordinator2"))
                && request.args.first().is_some_and(|arg| arg == "call")
                && request.args.get(1).is_some_and(|arg| {
                    matches!(
                        arg.to_str(),
                        Some("repository.list" | "deployment.list" | "health.summary")
                    )
                })
            {
                let operation = request.args[1].to_str().unwrap();
                let expected_params = if operation == "repository.list" {
                    "{\"limit\":1}"
                } else {
                    "{}"
                };
                if request.args
                    != ["call", operation, "--params", expected_params].map(OsString::from)
                {
                    return Err("candidate verification requires the exact v3 call argv".into());
                }
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: serde_json::json!({"protocol":3,"id":"x","ok":true,"data":{}})
                        .to_string(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.args == [OsString::from("--source-commit")] {
                return Ok(crate::install::CommandOutput {
                    success: true,
                    stdout: if request
                        .program
                        .components()
                        .any(|part| part.as_os_str() == "previous-release")
                    {
                        "b".repeat(40)
                    } else {
                        self.commit.clone()
                    },
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            }
            if request.program.file_name() == Some(std::ffi::OsStr::new("devcoordinator2"))
                && request.args == [OsString::from("call"), OsString::from("ping")]
            {
                if self
                    .retryable_ping_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Ok(crate::install::CommandOutput {
                        success: false,
                        stdout: "{\"protocol\":3,\"id\":\"x\",\"ok\":false,\"error\":{\"code\":\"daemon_unavailable\",\"message\":\"not ready\",\"detail\":\"\"}}\n".to_owned(),
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
                        let instance = request
                            .environment
                            .get(std::ffi::OsStr::new("DEVCOORDINATOR2_INSTANCE_ENV"))
                            .unwrap();
                        let socket = Path::new(instance)
                            .parent()
                            .unwrap()
                            .parent()
                            .unwrap()
                            .join("run/daemon.sock");
                        serde_json::json!({"protocol":3,"id":"x","ok":true,"data":{"source_commit":self.commit,"socket":socket}}).to_string()
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
            if request.program.file_name() == Some(std::ffi::OsStr::new("devcoordinator2")) {
                return Err(
                    "unexpected candidate control argv; legacy commands are refused".into(),
                );
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

    fn assert_candidate_read_calls(requests: &[CommandRequest]) {
        let actual = requests
            .iter()
            .filter(|request| {
                request.program.file_name() == Some(std::ffi::OsStr::new("devcoordinator2"))
                    && request.args != [OsString::from("--source-commit")]
            })
            .map(|request| request.args.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                vec!["call", "ping"],
                vec!["call", "repository.list", "--params", "{\"limit\":1}"],
                vec!["call", "deployment.list", "--params", "{}"],
                vec!["call", "health.summary", "--params", "{}"],
            ]
            .into_iter()
            .map(|args| args.into_iter().map(OsString::from).collect::<Vec<_>>())
            .collect::<Vec<_>>()
        );
    }

    struct HostWorld {
        _temporary: tempfile::TempDir,
        _listener: UnixListener,
        config: HostCutoverConfig,
        expected_owner: (u32, u32),
        old_daemon: String,
        old_cli: String,
        old_edge_unit: String,
        old_edge_env: String,
        commit: String,
    }

    impl HostWorld {
        fn rollback_edge_env(&self) -> String {
            let source = self
                .config
                .candidate_manifest
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("source");
            self.old_edge_env.replace(
                &format!("EDGE_CONSOLE_DIR={}/console", source.display()),
                &format!(
                    "EDGE_CONSOLE_DIR={}/console",
                    self.config.source_cache_dir.join("b".repeat(40)).display()
                ),
            )
        }
        fn rollback_edge_unit(&self) -> String {
            let source = self
                .config
                .candidate_manifest
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("source");
            self.old_edge_unit.replace(
                source.to_str().unwrap(),
                self.config
                    .source_cache_dir
                    .join("b".repeat(40))
                    .to_str()
                    .unwrap(),
            )
        }
    }

    fn host_world() -> HostWorld {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let source = root.join("source");
        std::fs::create_dir_all(source.join("deploy")).unwrap();
        std::fs::create_dir_all(source.join("edge")).unwrap();
        std::fs::create_dir_all(source.join("console")).unwrap();
        std::fs::create_dir_all(source.join(".git")).unwrap();
        std::fs::write(
            source.join("edge/devcoordinator2-edge.mjs"),
            "fixture edge source\n",
        )
        .unwrap();
        std::fs::write(
            source.join("console/index.html"),
            "fixture Console source\n",
        )
        .unwrap();
        std::fs::create_dir_all(source.join("target/release")).unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.tmpfiles.conf"),
            include_str!("../../../deploy/devcoordinator2.tmpfiles.conf"),
        )
        .unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2.service"),
            include_str!("../../../deploy/devcoordinator2.service"),
        )
        .unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2-edge.service"),
            include_str!("../../../deploy/devcoordinator2-edge.service"),
        )
        .unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2-tests.slice"),
            include_str!("../../../deploy/devcoordinator2-tests.slice"),
        )
        .unwrap();
        std::fs::write(
            source.join("deploy/devcoordinator2-postgres.service"),
            include_str!("../../../deploy/devcoordinator2-postgres.service"),
        )
        .unwrap();
        let pg_bin = root.join("postgres-bin");
        std::fs::create_dir(&pg_bin).unwrap();
        for name in [
            "postgres",
            "initdb",
            "psql",
            "pg_dump",
            "pg_restore",
            "pg_isready",
        ] {
            let path = pg_bin.join(name);
            std::fs::write(&path, "fixture tool").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let commit = "c".repeat(40);
        let mut receipts = Vec::new();
        for name in [
            "devcoordinator2",
            "devcoordinator2-executor",
            "devcoordinator2-tooling",
            "devcoordinator2-migrate-authority",
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
        let candidate_acceptance = root.join("etc/candidate-acceptance.json");
        let acceptance = install::postgres::CandidateAcceptance {
            schema: 1,
            result: "passed".into(),
            source_commit: manifest.source_commit.clone(),
            binaries: manifest
                .binaries
                .iter()
                .map(|binary| (binary.name.clone(), binary.sha256.clone()))
                .collect(),
            checks: install::postgres::ACCEPTANCE_CHECKS
                .iter()
                .map(|name| (*name).into())
                .collect(),
        };
        install::atomic_file(
            &candidate_acceptance,
            &serde_json::to_vec(&acceptance).unwrap(),
            0o600,
            Some(expected_owner),
        )
        .unwrap();

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
            .execute_batch(include_str!(
                "../../persistence/tests/fixtures/sqlite-schema-34/schema.sql"
            ))
            .unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO meta(key,value) VALUES('schema_version','34')",
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
        let tests_slice_unit_path = system.join("devcoordinator2-tests.slice");
        let cli_link = bin.join("devcoordinator2");
        let tooling_link = bin.join("devcoordinator2-tooling");
        let installed_manifest = root.join("etc/install-manifest.json");
        let old_daemon = "ExecStart=/usr/bin/python3 -m devcoordinator2.daemon\n".to_owned();
        let old_cli = "#!/usr/bin/python3\n".to_owned();
        let old_edge_unit = format!(
            "[Service]\nExecStart=/usr/bin/node {}\nReadOnlyPaths={}\n",
            source.join("edge/devcoordinator2-edge.mjs").display(),
            source.display()
        );
        let old_edge_env = format!(
            "EDGE_BASE_DOMAIN=example.test\nUNRELATED=retained-private-setting\nEDGE_UI_SOURCE_REVISION=git:{}\nEDGE_CONSOLE_DIR={}/console\n",
            "b".repeat(40),
            source.display()
        );
        let edge_env = root.join("etc/edge.env");
        std::fs::write(&edge_env, &old_edge_env).unwrap();
        std::fs::set_permissions(&edge_env, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::write(&daemon_unit_path, &old_daemon).unwrap();
        std::fs::write(&edge_unit_path, &old_edge_unit).unwrap();
        std::fs::write(&tests_slice_unit_path, "old test slice\n").unwrap();
        std::fs::write(&cli_link, &old_cli).unwrap();
        std::fs::write(&tooling_link, "old tooling\n").unwrap();
        let previous = source.join("target/previous-release");
        std::fs::create_dir_all(&previous).unwrap();
        let binaries = [
            "devcoordinator2",
            "devcoordinator2-executor",
            "devcoordinator2-tooling",
        ]
        .into_iter()
        .map(|name| {
            let path = previous.join(name);
            std::fs::write(&path, "prior fixture binary").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            install::BinaryReceipt {
                name: name.into(),
                path: path.to_string_lossy().into_owned(),
                sha256: install::hash_file(&path).unwrap(),
                bytes: std::fs::metadata(&path).unwrap().len(),
                source_commit: "b".repeat(40),
            }
        })
        .collect();
        install::write_manifest(
            &installed_manifest,
            &InstallManifest {
                schema: 1,
                product: "devcoordinator2".into(),
                package_version: env!("CARGO_PKG_VERSION").into(),
                source_root: source.to_string_lossy().into_owned(),
                source_commit: "b".repeat(40),
                built_at: "fixture".into(),
                binaries,
            },
            expected_owner,
        )
        .unwrap();
        HostWorld {
            config: HostCutoverConfig {
                candidate_acceptance,
                postgres: install::postgres::PostgresSetup {
                    account: "fixture-pg".into(),
                    database_name: "devcoordinator2_fixture".into(),
                    bin_dir: pg_bin,
                    data_dir: root.join("pg-state/data"),
                    socket_dir: root.join("pg-run"),
                    config_path: root.join("etc/postgres.json"),
                    unit_path: root.join("system/devcoordinator2-postgres.service"),
                    unit: "devcoordinator2-postgres.service".into(),
                },
                candidate_manifest,
                installed_manifest,
                source_cache_dir: root.join("rollback-sources"),
                tmpfiles_path: root.join("etc/tmpfiles.conf"),
                transaction_dir: root.join("transaction"),
                runtime_dir,
                socket_path,
                database_path,
                daemon_unit_path,
                edge_unit_path,
                edge_env,
                tests_slice_unit_path,
                cli_link,
                tooling_link,
                systemctl: "/fake/systemctl".into(),
                ss: "/fake/ss".into(),
                instance_env: root.join("etc/instance.env"),
                daemon_unit: "devcoordinator2.service".to_owned(),
                edge_unit: "devcoordinator2-edge.service".to_owned(),
                edge_state_dir: root.join("edge-state"),
                canary: false,
                ephemeral_services: false,
            },
            expected_owner,
            old_daemon,
            old_cli,
            old_edge_unit,
            old_edge_env,
            commit,
            _listener: listener,
            _temporary: temporary,
        }
    }

    #[test]
    fn bootstrap_snapshot_without_database_is_explicit_and_recoverable() {
        let world = host_world();
        for path in [
            &world.config.database_path,
            &world.config.daemon_unit_path,
            &world.config.edge_unit_path,
            &world.config.tests_slice_unit_path,
            &world.config.cli_link,
            &world.config.tooling_link,
            &world.config.installed_manifest,
        ] {
            std::fs::remove_file(path).unwrap();
        }
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..Default::default()
        });
        let host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        let snapshot = host.capture_installation_for(true).unwrap();
        assert_eq!(snapshot.schema, 2);
        assert!(snapshot.bootstrap);
        assert!(snapshot.backup_sha256.is_empty());
        assert!(host.capture_installation_for(false).is_err());
        let recovery = RecoveryConfig {
            transaction_dir: world.config.transaction_dir.clone(),
            source_root: world._temporary.path().join("source"),
            edge_env: world.config.edge_env.clone(),
            runtime_dir: world.config.runtime_dir.clone(),
            socket_path: world.config.socket_path.clone(),
            database_path: world.config.database_path.clone(),
            systemctl: world.config.systemctl.clone(),
            daemon_unit: world.config.daemon_unit.clone(),
            edge_unit: world.config.edge_unit.clone(),
            edge_state_dir: world.config.edge_state_dir.clone(),
        };
        assert!(
            !recover_host(&recovery, runner.as_ref(), world.expected_owner)
                .unwrap()
                .database_restored
        );
        assert!(!world.config.database_path.exists());
        assert!(
            recover_host(&recovery, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .contains("not an in-progress")
        );
    }

    #[test]
    fn bootstrap_cleanup_does_not_ignore_real_stop_failure() {
        let runner = HostFake {
            loaded_units: true,
            fail_stop: true,
            ..Default::default()
        };
        assert!(
            cleanup_bootstrap_units(&runner, Path::new("/fake/systemctl"), &["test.service"])
                .is_err()
        );
        assert!(
            !runner
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.args.first() == Some(&OsString::from("disable")))
        );
    }

    #[test]
    fn bootstrap_partial_install_failure_cleans_unloaded_units_and_can_recover() {
        let mut world = host_world();
        world.config.canary = true;
        for path in [
            &world.config.daemon_unit_path,
            &world.config.edge_unit_path,
            &world.config.tests_slice_unit_path,
            &world.config.cli_link,
            &world.config.tooling_link,
            &world.config.installed_manifest,
            &world.config.socket_path,
        ] {
            std::fs::remove_file(path).unwrap();
        }
        let original = std::fs::read(&world.config.database_path).unwrap();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fail_reload: AtomicUsize::new(1),
            fail_stop: true,
            ..Default::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        let error = host.bootstrap().unwrap_err();
        assert!(error.contains("new installation removed"), "{error}");
        for path in [
            &world.config.daemon_unit_path,
            &world.config.edge_unit_path,
            &world.config.tests_slice_unit_path,
            &world.config.cli_link,
            &world.config.tooling_link,
            &world.config.installed_manifest,
        ] {
            assert!(!path.exists());
        }
        let recovery = RecoveryConfig {
            transaction_dir: world.config.transaction_dir.clone(),
            source_root: world._temporary.path().join("source"),
            edge_env: world.config.edge_env.clone(),
            runtime_dir: world.config.runtime_dir.clone(),
            socket_path: world.config.socket_path.clone(),
            database_path: world.config.database_path.clone(),
            systemctl: world.config.systemctl.clone(),
            daemon_unit: world.config.daemon_unit.clone(),
            edge_unit: world.config.edge_unit.clone(),
            edge_state_dir: world.config.edge_state_dir.clone(),
        };
        assert!(
            recover_host(&recovery, runner.as_ref(), world.expected_owner)
                .unwrap_err()
                .contains("not an in-progress")
        );
        assert_eq!(
            std::fs::read(&world.config.database_path).unwrap(),
            original
        );
        assert!(
            world
                .config
                .transaction_dir
                .join("authority-before.sqlite3")
                .is_file()
        );
    }

    #[test]
    fn bootstrap_requires_absent_installation_and_canary() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            ..Default::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        assert!(host.bootstrap().unwrap_err().contains("--canary"));
        host.config.canary = true;
        assert!(
            host.bootstrap()
                .unwrap_err()
                .contains("absent installation target")
        );
        assert!(!world.config.transaction_dir.exists());
    }

    #[test]
    fn bootstrap_preserves_imported_database_on_success_and_failure() {
        for fail_ping in [false, true] {
            let mut world = host_world();
            world.config.canary = true;
            std::fs::remove_file(world.config.runtime_dir.join(ACTIVITY_FILE)).unwrap();
            for path in [
                &world.config.daemon_unit_path,
                &world.config.edge_unit_path,
                &world.config.tests_slice_unit_path,
                &world.config.cli_link,
                &world.config.tooling_link,
                &world.config.installed_manifest,
                &world.config.socket_path,
            ] {
                std::fs::remove_file(path).unwrap();
            }
            let original = std::fs::read(&world.config.database_path).unwrap();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fail_ping,
                ..Default::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                    .unwrap();
            let result = host.bootstrap();
            assert_eq!(result.is_err(), fail_ping, "{result:?}");
            assert!(
                world
                    .config
                    .transaction_dir
                    .join("bootstrap-fence.json")
                    .is_file()
            );
            assert!(
                !world
                    .config
                    .runtime_dir
                    .join("daemon.pre-cutover.sock")
                    .exists()
            );
            assert!(!world.config.runtime_dir.join(ACTIVITY_FILE).exists());
            assert_eq!(
                std::fs::read(&world.config.database_path).unwrap(),
                original
            );
            assert!(
                world
                    .config
                    .transaction_dir
                    .join("authority-before.sqlite3")
                    .exists()
            );
            let snapshot =
                read_snapshot(&world.config.transaction_dir, world.expected_owner.0).unwrap();
            assert!(snapshot.bootstrap);
            assert_eq!(
                snapshot.status,
                if fail_ping {
                    "rolled_back"
                } else {
                    "committed"
                }
            );
            assert_eq!(world.config.daemon_unit_path.exists(), !fail_ping);
            assert_eq!(world.config.tests_slice_unit_path.exists(), !fail_ping);
            if !fail_ping {
                let recovery = RecoveryConfig {
                    transaction_dir: world.config.transaction_dir.clone(),
                    source_root: world._temporary.path().join("source"),
                    edge_env: world.config.edge_env.clone(),
                    runtime_dir: world.config.runtime_dir.clone(),
                    socket_path: world.config.socket_path.clone(),
                    database_path: world.config.database_path.clone(),
                    systemctl: world.config.systemctl.clone(),
                    daemon_unit: world.config.daemon_unit.clone(),
                    edge_unit: world.config.edge_unit.clone(),
                    edge_state_dir: world.config.edge_state_dir.clone(),
                };
                assert!(
                    recover_host(&recovery, runner.as_ref(), world.expected_owner)
                        .unwrap_err()
                        .contains("exact committed activation fence is unavailable")
                );
                assert!(world.config.daemon_unit_path.exists());
                assert_eq!(
                    std::fs::read(&world.config.database_path).unwrap(),
                    original
                );
            }
        }
    }

    #[test]
    fn bootstrap_rejects_existing_active_or_invalid_test_receipt() {
        for (receipt, expected_error) in [
            (
                r#"{"schema":1,"active":[{"run":"existing"}]}"#,
                "bootstrap blocked by existing test activity",
            ),
            (
                r#"{"schema":2,"active":[]}"#,
                "test activity receipt has an unsupported schema",
            ),
        ] {
            let mut world = host_world();
            world.config.canary = true;
            for path in [
                &world.config.daemon_unit_path,
                &world.config.edge_unit_path,
                &world.config.tests_slice_unit_path,
                &world.config.cli_link,
                &world.config.tooling_link,
                &world.config.installed_manifest,
                &world.config.socket_path,
            ] {
                std::fs::remove_file(path).unwrap();
            }
            std::fs::write(world.config.runtime_dir.join(ACTIVITY_FILE), receipt).unwrap();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                ..Default::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
            let error = host.bootstrap().unwrap_err();
            assert!(error.contains(expected_error), "{error}");
            assert!(!world.config.transaction_dir.exists());
            assert!(!world.config.daemon_unit_path.exists());
            assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        }
    }

    #[test]
    fn concrete_host_adapter_installs_verified_files_and_commits_snapshot() {
        for retain_legacy_sources in [false, true] {
            verify_host_adapter_finalization(retain_legacy_sources);
        }
    }

    fn verify_host_adapter_finalization(retain_legacy_sources: bool) {
        let mut world = host_world();
        let transactions = world
            .config
            .database_path
            .parent()
            .unwrap()
            .join("install-transactions");
        std::fs::create_dir(&transactions).unwrap();
        world.config.transaction_dir = transactions.join("candidate");
        let edge_env = world.config.instance_env.with_file_name("edge.env");
        let prior_edge_metadata = std::fs::metadata(&edge_env).unwrap();
        let prior_rule =
            "d /var/lib/devcoordinator2 0750 root root -\n# retain this instance setting\n";
        std::fs::write(&world.config.tmpfiles_path, prior_rule).unwrap();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        for args in [
            vec!["ping"],
            vec!["repository.list", "--params", "{\"limit\":1}"],
            vec!["call", "repository.list", "--params", "{}"],
            vec!["call", "deployment.list", "--params", "{}", "extra"],
            vec!["call", "health.summary"],
        ] {
            let malformed = CommandRequest {
                program: host.installed_binary("devcoordinator2").unwrap(),
                args: args.into_iter().map(OsString::from).collect(),
                environment: Default::default(),
                clear_environment: true,
            };
            assert!(HostFake::default().run(&malformed).is_err());
        }
        let receipt = activate(&mut host).unwrap();
        assert_candidate_read_calls(&runner.requests.lock().unwrap());
        let daemon_unit = std::fs::read_to_string(&world.config.daemon_unit_path).unwrap();
        assert!(daemon_unit.contains(&format!(
            "EnvironmentFile=-{}\n",
            world.config.instance_env.display()
        )));
        assert!(daemon_unit.contains(&format!(
            "Environment=DEVCOORDINATOR2_INSTANCE_ENV={}\n",
            world.config.instance_env.display()
        )));
        let edge_unit = std::fs::read_to_string(&world.config.edge_unit_path).unwrap();
        assert!(edge_unit.contains(&format!(
            "EnvironmentFile={}\n",
            world.config.edge_env.display()
        )));
        assert!(edge_unit.contains(&format!(
            "Environment=EDGE_STATE_DIR={}\n",
            world.config.edge_state_dir.display()
        )));
        assert!(edge_unit.contains(&format!(
            "ReadWritePaths={}\n",
            world.config.edge_state_dir.display()
        )));
        let edge_directives = edge_unit
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        assert!(
            edge_directives
                .iter()
                .all(|line| !line.contains("/etc/devcoordinator2/")
                    && !line.contains("/var/lib/devcoordinator2")
                    && *line != "StateDirectory=devcoordinator2-edge")
        );
        assert!(daemon_unit.lines().any(|line| line == "ProtectSystem=full"));
        assert!(daemon_unit.contains(&format!(
            "ReadWritePaths=/home {} {} {}\n",
            world.config.database_path.parent().unwrap().display(),
            world.config.runtime_dir.display(),
            world.config.instance_env.parent().unwrap().display(),
        )));
        assert_eq!(receipt.status, "activated_awaiting_final_acceptance");
        assert_eq!(
            std::fs::read_to_string(&edge_env).unwrap(),
            world.old_edge_env.replace(&"b".repeat(40), &world.commit)
        );
        let edge_metadata = std::fs::metadata(&edge_env).unwrap();
        assert_eq!(
            (
                edge_metadata.uid(),
                edge_metadata.gid(),
                edge_metadata.mode()
            ),
            (
                prior_edge_metadata.uid(),
                prior_edge_metadata.gid(),
                prior_edge_metadata.mode()
            )
        );
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains("retained-private-setting")
        );
        assert!(world.config.database_path.is_file());
        assert!(
            world
                .config
                .transaction_dir
                .join("authority-before.pgdump")
                .is_file()
        );
        assert!(
            world
                .config
                .transaction_dir
                .join("retirement-pending.json")
                .is_file()
        );
        assert!(world.config.postgres.config_path.is_file());
        assert!(
            !receipt
                .checks
                .iter()
                .any(|check| check == "legacy-authority-source-retired")
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
        assert!(
            Path::new(&receipt.backup).is_file(),
            "rollback source stays protected until post-activation live acceptance"
        );
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
        assert_eq!(snapshot.schema, 3);
        assert_eq!(
            snapshot.backup_sha256,
            install::hash_file(&world.config.transaction_dir.join("authority-before.pgdump"))
                .unwrap()
        );
        assert!(
            !world
                .config
                .runtime_dir
                .join("daemon.pre-cutover.sock")
                .exists()
        );
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        let retained_sha = install::hash_file(&world.config.database_path).unwrap();
        let commands_before = runner.requests.lock().unwrap().len();
        let error =
            rollback(&mut host, &snapshot, &receipt.backup, "live failure".into()).unwrap_err();
        assert!(error.contains("preserve its new writes"));
        assert_eq!(commands_before, runner.requests.lock().unwrap().len());
        assert_eq!(
            retained_sha,
            install::hash_file(&world.config.database_path).unwrap()
        );
        let finalization = FinalizationConfig {
            transaction_dir: world.config.transaction_dir.clone(),
            installed_manifest: world.config.installed_manifest.clone(),
            live_acceptance: world.config.transaction_dir.join("live-acceptance.json"),
            retain_legacy_sources,
        };
        assert!(finalize_host(&finalization, runner.as_ref(), world.expected_owner).is_err());
        assert!(world.config.database_path.is_file());
        let mut live = LiveAcceptance {
            schema: 1,
            result: "passed".into(),
            source_commit: host.manifest.source_commit.clone(),
            edge_source_commit: host.manifest.source_commit.clone(),
            binaries: host
                .manifest
                .binaries
                .iter()
                .map(|binary| (binary.name.clone(), binary.sha256.clone()))
                .collect(),
            checked_at_ms: (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                as u64,
            checks: vec!["critical-api".into()],
            evidence: vec!["fixture:live-journey-receipt".into()],
        };
        install::atomic_file(
            &finalization.live_acceptance,
            &serde_json::to_vec(&live).unwrap(),
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        assert!(finalize_host(&finalization, runner.as_ref(), world.expected_owner).is_err());
        assert!(world.config.database_path.is_file());
        live.checks = finalize::LIVE_CHECKS
            .iter()
            .map(|check| (*check).into())
            .collect();
        install::atomic_file(
            &finalization.live_acceptance,
            &serde_json::to_vec(&live).unwrap(),
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        let pending_marker =
            std::fs::read(world.config.transaction_dir.join("retirement-pending.json")).unwrap();
        let finalized =
            finalize_host(&finalization, runner.as_ref(), world.expected_owner).unwrap();
        assert_eq!(finalized.status, "finalized");
        assert_eq!(
            finalized.retired_sources,
            if retain_legacy_sources { 0 } else { 2 }
        );
        assert_eq!(
            finalized.retained_sources,
            if retain_legacy_sources { 2 } else { 0 }
        );
        assert_eq!(world.config.database_path.exists(), retain_legacy_sources);
        assert_eq!(Path::new(&receipt.backup).exists(), retain_legacy_sources);
        if retain_legacy_sources {
            assert_eq!(
                retained_sha,
                install::hash_file(&world.config.database_path).unwrap()
            );
            assert!(
                world
                    .config
                    .transaction_dir
                    .join("retained-sources.json")
                    .is_file()
            );
        }
        assert!(
            !world
                .config
                .transaction_dir
                .join("retirement-pending.json")
                .exists()
        );
        if retain_legacy_sources {
            assert_eq!(
                finalize_host(&finalization, runner.as_ref(), world.expected_owner).unwrap(),
                finalized
            );
            let saved_source = std::fs::read(&world.config.database_path).unwrap();
            std::fs::write(&world.config.database_path, b"changed retained source").unwrap();
            assert!(finalize_host(&finalization, runner.as_ref(), world.expected_owner).is_err());
            std::fs::write(&world.config.database_path, saved_source).unwrap();
        }
        // A crash after the terminal receipt but before removing the marker
        // must finish that last exact cleanup on an idempotent retry.
        install::atomic_file(
            &world.config.transaction_dir.join("retirement-pending.json"),
            &pending_marker,
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        assert_eq!(
            finalize_host(&finalization, runner.as_ref(), world.expected_owner).unwrap(),
            finalized
        );
        assert!(
            !world
                .config
                .transaction_dir
                .join("retirement-pending.json")
                .exists()
        );
    }

    #[test]
    fn native_upgrade_restores_into_a_fresh_candidate_and_retains_prior_authority_on_failure() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut first =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        activate(&mut first).unwrap();
        let prior_config = std::fs::read(&world.config.postgres.config_path).unwrap();
        let prior_marker = std::fs::read(&first.authority_marker_path).unwrap();
        let mut candidate = world.config.clone();
        candidate.transaction_dir = world._temporary.path().join("native-upgrade");
        std::fs::remove_file(&first.authority_marker_path).unwrap();
        assert!(
            HostCutover::new_owned(candidate.clone(), runner.clone(), world.expected_owner)
                .is_err()
        );
        install::atomic_file(
            &first.authority_marker_path,
            &prior_marker,
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        let unfinished = FinalizationReceipt {
            schema: 1,
            status: "retiring".into(),
            source_commit: first.manifest.source_commit.clone(),
            live_acceptance_sha256: "1".repeat(64),
            postgres_archive_sha256: "2".repeat(64),
            retired_sources: 0,
            retained_sources: 0,
        };
        let prior_finalization = world.config.transaction_dir.join("finalization.json");
        install::atomic_file(
            &prior_finalization,
            &serde_json::to_vec(&unfinished).unwrap(),
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        assert!(
            HostCutover::new_owned(candidate.clone(), runner.clone(), world.expected_owner)
                .is_err()
        );
        std::fs::remove_file(prior_finalization).unwrap();
        // The fixture's replacement daemon binds the newly exposed endpoint.
        std::fs::write(&candidate.socket_path, b"prior native daemon endpoint").unwrap();
        let failing = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            fail_ping: true,
            ..HostFake::default()
        });
        let mut upgrade =
            HostCutover::new_owned(candidate.clone(), failing.clone(), world.expected_owner)
                .unwrap();
        assert_eq!(upgrade.authority, AuthorityKind::Postgres);
        assert_ne!(
            upgrade.config.postgres.database_name,
            first.config.postgres.database_name
        );
        assert!(
            activate(&mut upgrade)
                .unwrap_err()
                .contains("without replacing the intact database")
        );
        assert_eq!(
            std::fs::read(&candidate.postgres.config_path).unwrap(),
            prior_config
        );
        assert_eq!(
            std::fs::read(&upgrade.authority_marker_path).unwrap(),
            prior_marker
        );
        let modes = failing
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.args.first().is_some_and(|arg| arg == "--mode"))
            .map(|request| request.args[1].to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(modes.iter().any(|mode| mode == "restore-candidate"));
        assert!(modes.iter().any(|mode| mode == "migrate"));
        assert!(!modes.iter().any(|mode| mode == "import"));
        assert!(
            world.config.database_path.is_file(),
            "inherited SQLite retirement stays protected"
        );
        let mut cold_config = world.config.clone();
        cold_config.transaction_dir = world._temporary.path().join("native-upgrade-interrupted");
        let mut cold =
            HostCutover::new_owned(cold_config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        cold.fence_legacy_socket().unwrap();
        cold.stop_legacy().unwrap();
        cold.backup_database().unwrap();
        cold.capture_installation().unwrap();
        cold.install_rust().unwrap();
        assert_ne!(
            std::fs::read(&cold_config.postgres.config_path).unwrap(),
            prior_config
        );
        let recovered = recover_host(
            &RecoveryConfig {
                transaction_dir: cold_config.transaction_dir.clone(),
                runtime_dir: cold_config.runtime_dir.clone(),
                socket_path: cold_config.socket_path.clone(),
                database_path: cold_config.database_path.clone(),
                systemctl: cold_config.systemctl.clone(),
                daemon_unit: cold_config.daemon_unit.clone(),
                edge_unit: cold_config.edge_unit.clone(),
                edge_state_dir: cold_config.edge_state_dir.clone(),
                source_root: PathBuf::from(&cold.manifest.source_root),
                edge_env: cold_config.edge_env.clone(),
            },
            runner.as_ref(),
            world.expected_owner,
        )
        .unwrap();
        assert_eq!(recovered.status, "recovered");
        assert!(!recovered.database_restored);
        assert_eq!(
            std::fs::read(&cold_config.postgres.config_path).unwrap(),
            prior_config
        );
        assert_eq!(
            std::fs::read(&cold.authority_marker_path).unwrap(),
            prior_marker
        );
        let mut successful_config = world.config.clone();
        successful_config.transaction_dir = world._temporary.path().join("native-upgrade-retry");
        let mut successful =
            HostCutover::new_owned(successful_config, runner.clone(), world.expected_owner)
                .unwrap();
        let drain = successful.close_admission().unwrap();
        successful.wait_for_quiescence(&drain).unwrap();
        successful.fence_legacy_socket().unwrap();
        successful.wait_for_connections().unwrap();
        assert!(!successful.deployment_is_applying().unwrap());
        successful.stop_legacy().unwrap();
        successful.backup_database().unwrap();
        let prepared = successful.capture_installation().unwrap();
        successful.install_rust().unwrap();
        successful.start_rust().unwrap();
        successful.verify_live().unwrap();
        // Model process death after the durable native commitment and before
        // the final fence unlink. No old installation may be restored now.
        successful.retain_activation_commit(&prepared).unwrap();
        end_drain(&drain).unwrap();
        let recovery = RecoveryConfig {
            transaction_dir: successful.config.transaction_dir.clone(),
            runtime_dir: successful.config.runtime_dir.clone(),
            socket_path: successful.config.socket_path.clone(),
            database_path: successful.config.database_path.clone(),
            systemctl: successful.config.systemctl.clone(),
            daemon_unit: successful.config.daemon_unit.clone(),
            edge_unit: successful.config.edge_unit.clone(),
            edge_state_dir: successful.config.edge_state_dir.clone(),
            source_root: PathBuf::from(&successful.manifest.source_root),
            edge_env: successful.config.edge_env.clone(),
        };
        let current = read_json_file(&successful.authority_marker_path)
            .unwrap()
            .unwrap();
        let mut wrong = current.clone();
        wrong["database_name"] = Value::String("another_owned_candidate".into());
        install::atomic_file(
            &successful.authority_marker_path,
            &serde_json::to_vec(&wrong).unwrap(),
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        assert!(recover_host(&recovery, runner.as_ref(), world.expected_owner).is_err());
        assert!(
            successful
                .config
                .runtime_dir
                .join("daemon.pre-cutover.sock")
                .exists()
        );
        install::atomic_file(
            &successful.authority_marker_path,
            &serde_json::to_vec(&current).unwrap(),
            0o600,
            Some(world.expected_owner),
        )
        .unwrap();
        let unchanged_sqlite = install::hash_file(&world.config.database_path).unwrap();
        let before_recovery = runner.requests.lock().unwrap().len();
        let receipt = recover_host(&recovery, runner.as_ref(), world.expected_owner).unwrap();
        assert_candidate_read_calls(&runner.requests.lock().unwrap()[before_recovery..]);
        assert_eq!(receipt.status, "activated_awaiting_final_acceptance");
        assert!(!receipt.database_restored);
        assert_eq!(
            install::hash_file(&world.config.database_path).unwrap(),
            unchanged_sqlite
        );
        assert!(
            !successful
                .config
                .runtime_dir
                .join("daemon.pre-cutover.sock")
                .exists()
        );
        assert!(
            successful
                .config
                .transaction_dir
                .join("activation-recovery.json")
                .is_file()
        );
        assert!(
            !runner
                .requests
                .lock()
                .unwrap()
                .iter()
                .skip(before_recovery)
                .any(
                    |request| request.args.first().is_some_and(|arg| arg == "stop")
                        || request.args.windows(2).any(|pair| pair[0] == "--mode"
                            && matches!(pair[1].to_str(), Some("import" | "restore-candidate")))
                )
        );
        assert!(
            successful
                .config
                .transaction_dir
                .join("authority-before.pgdump")
                .is_file()
        );
        assert!(
            !successful
                .config
                .transaction_dir
                .join("authority-before.sqlite3")
                .exists()
        );
        let pending = read_json_file(
            &successful
                .config
                .transaction_dir
                .join("retirement-pending.json"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(pending["retirement_targets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn postgres_preparation_preserves_unknown_data_and_reuses_only_its_private_binding() {
        let world = host_world();
        let postgres = world.config.postgres.bin_dir.join("postgres");
        let saved_postgres = world.config.postgres.bin_dir.join("postgres.fixture-saved");
        std::fs::rename(&postgres, &saved_postgres).unwrap();
        let preflight_runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let error = HostCutover::new_owned(
            world.config.clone(),
            preflight_runner.clone(),
            world.expected_owner,
        )
        .err()
        .unwrap();
        assert!(error.contains("required before cutover"));
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        assert!(
            !preflight_runner
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.program == world.config.systemctl)
        );
        std::fs::rename(&saved_postgres, &postgres).unwrap();
        let runner = HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        };
        let mut setup = world.config.postgres.clone();
        setup.unit = "devcoordinator2-rustint-fixture-postgres.service".into();
        let source = PathBuf::from(
            read_json_file(&world.config.candidate_manifest)
                .unwrap()
                .unwrap()["source_root"]
                .as_str()
                .unwrap(),
        );
        std::fs::create_dir_all(&setup.data_dir).unwrap();
        std::fs::write(setup.data_dir.join("PG_VERSION"), "17\n").unwrap();
        std::fs::write(setup.data_dir.join("valuable"), b"preserve these bytes").unwrap();
        let before = std::fs::metadata(&setup.data_dir).unwrap().mode();
        assert!(
            setup
                .prepare(&source, &runner, world.expected_owner)
                .is_err()
        );
        assert_eq!(
            std::fs::read(setup.data_dir.join("valuable")).unwrap(),
            b"preserve these bytes"
        );
        assert_eq!(std::fs::metadata(&setup.data_dir).unwrap().mode(), before);
        std::fs::remove_file(setup.data_dir.join("PG_VERSION")).unwrap();
        std::fs::remove_file(setup.data_dir.join("valuable")).unwrap();
        setup
            .prepare(&source, &runner, world.expected_owner)
            .unwrap();
        setup
            .prepare(&source, &runner, world.expected_owner)
            .unwrap();
        let config = std::fs::read_to_string(setup.data_dir.join("postgresql.conf")).unwrap();
        let private_config = read_json_file(&setup.config_path).unwrap().unwrap();
        assert_eq!(
            private_config["data_directory"].as_str(),
            setup.data_dir.to_str()
        );
        assert!(config.contains("listen_addresses = ''"));
        assert!(config.contains("log_parameter_max_length_on_error = 0"));
        assert!(config.contains("log_statement = 'none'"));
        assert!(config.contains("log_min_error_statement = 'panic'"));
        assert!(config.contains("log_error_verbosity = 'terse'"));
        let private_before = install::hash_file(&setup.config_path).unwrap();
        let marker = setup
            .data_dir
            .parent()
            .unwrap()
            .join("authority-install.json");
        std::fs::write(&marker, b"{\"schema\":1,\"product\":\"another-authority\"}").unwrap();
        std::fs::set_permissions(&setup.data_dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            setup
                .prepare(&source, &runner, world.expected_owner)
                .is_err()
        );
        assert_eq!(
            std::fs::metadata(&setup.data_dir).unwrap().mode() & 0o777,
            0o750
        );
        assert_eq!(
            install::hash_file(&setup.config_path).unwrap(),
            private_before
        );
        assert!(
            !std::fs::read_to_string(&setup.unit_path)
                .unwrap()
                .contains("Before=devcoordinator2.service")
        );
    }

    #[test]
    fn concrete_host_adapter_rejects_candidate_binary_drift_after_admission_drains() {
        for failure in ["binary", "source", "dirty"] {
            let world = host_world();
            let changed_commit = "d".repeat(40);
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                checkout_commit: (failure == "source").then_some(changed_commit.clone()),
                dirty_checkout: failure == "dirty",
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                    .unwrap();
            if failure == "binary" {
                let manifest: crate::install::InstallManifest = serde_json::from_slice(
                    &std::fs::read(&world.config.candidate_manifest).unwrap(),
                )
                .unwrap();
                std::fs::write(&manifest.binaries[0].path, b"candidate-built-after-plan").unwrap();
            }

            let error = activate(&mut host).unwrap_err();
            let expected_error = match failure {
                "binary" => "candidate release binaries changed",
                "source" => "Console source no longer matches",
                _ => "live checkout must be clean",
            };
            assert!(error.contains(expected_error), "{failure}: {error}");
            assert!(!error.contains("retained-private-setting"));
            let environment = std::fs::read_to_string(&world.config.edge_env).unwrap();
            assert!(environment.contains("UNRELATED=retained-private-setting\n"));
            assert!(
                environment.contains(&format!("EDGE_UI_SOURCE_REVISION=git:{}\n", "b".repeat(40)))
            );
            assert_eq!(
                environment,
                if failure == "source" {
                    world.rollback_edge_env()
                } else {
                    world.old_edge_env.clone()
                }
            );
            assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
            assert!(
                !world
                    .config
                    .runtime_dir
                    .join("daemon.pre-cutover.sock")
                    .exists()
            );
            assert_eq!(
                std::fs::read_to_string(&world.config.daemon_unit_path).unwrap(),
                world.old_daemon
            );
            assert_eq!(
                std::fs::read_to_string(&world.config.edge_unit_path).unwrap(),
                if failure == "source" {
                    world.rollback_edge_unit()
                } else {
                    world.old_edge_unit.clone()
                }
            );
            let prior_manifest: Value =
                serde_json::from_slice(&std::fs::read(&world.config.installed_manifest).unwrap())
                    .unwrap();
            assert_eq!(prior_manifest["source_commit"], "b".repeat(40));
            if failure == "dirty" {
                assert!(
                    !runner.requests.lock().unwrap().iter().any(|request| {
                        request.args.first() == Some(&OsString::from("restart"))
                    })
                );
                assert!(!world.config.transaction_dir.exists());
            }
        }
    }

    #[test]
    fn concrete_host_adapter_activates_supported_schemas_and_rejects_unknown_schemas() {
        let world = host_world();
        let connection = Connection::open(&world.config.database_path).unwrap();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        for unsupported in ["14", "15", "33", "35", "36", "invalid"] {
            connection
                .execute(
                    "UPDATE meta SET value=?1 WHERE key='schema_version'",
                    [unsupported],
                )
                .unwrap();
            assert!(
                host.deployment_is_applying()
                    .unwrap_err()
                    .contains("schema")
            );
            assert!(
                !runner
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.args.first().is_some_and(|action| action == "stop"))
            );
        }
        connection
            .execute("UPDATE meta SET value='34' WHERE key='schema_version'", [])
            .unwrap();
        drop(connection);
        let receipt = activate(&mut host).unwrap();
        assert_eq!(receipt.status, "activated_awaiting_final_acceptance");
        assert!(world.config.database_path.is_file());
        assert!(
            world
                .config
                .transaction_dir
                .join("authority-before.pgdump")
                .is_file()
        );
        assert!(
            world
                .config
                .transaction_dir
                .join("retirement-pending.json")
                .is_file()
        );
    }

    #[test]
    fn concrete_host_adapter_waits_for_the_daemon_socket_to_bind() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            retryable_ping_failures: AtomicUsize::new(1),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();

        let receipt = activate(&mut host).unwrap();

        assert_eq!(receipt.status, "activated_awaiting_final_acceptance");
        assert_eq!(
            runner
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.args == [OsString::from("call"), OsString::from("ping")])
                .count(),
            2
        );
    }

    #[test]
    fn concrete_host_adapter_requires_backup_restore_after_schema_upgrade() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
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
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
        host.stop_legacy().unwrap();
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

        // A failure after the durable commit must never select SQLite again.
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
        host.stop_legacy().unwrap();
        host.backup_database().unwrap();
        let snapshot = host.capture_installation().unwrap();
        host.fence_legacy_socket().unwrap();
        host.install_rust().unwrap();
        host.start_rust().unwrap();
        host.verify_live().unwrap();
        std::fs::create_dir(&host.authority_marker_path).unwrap();
        assert!(host.commit_activation(&snapshot).is_err());
        assert!(host.writes_admitted());
        assert_eq!(
            read_snapshot(&world.config.transaction_dir, world.expected_owner.0)
                .unwrap()
                .status,
            "committed"
        );
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
        let schema = 34;
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
            fragment: world.config.daemon_unit_path.clone(),
            fail_ping: true,
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
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

    #[test]
    fn concrete_host_adapter_restores_python_files_and_socket_on_failed_ping() {
        let world = host_world();
        let edge_env = world.config.instance_env.with_file_name("edge.env");
        let prior_edge_metadata = std::fs::metadata(&edge_env).unwrap();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
            fail_ping: true,
            ..HostFake::default()
        });
        let mut host =
            HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                .unwrap();
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
        assert_eq!(
            std::fs::read_to_string(&edge_env).unwrap(),
            world.rollback_edge_env()
        );
        let edge_metadata = std::fs::metadata(&edge_env).unwrap();
        assert_eq!(
            (
                edge_metadata.uid(),
                edge_metadata.gid(),
                edge_metadata.mode()
            ),
            (
                prior_edge_metadata.uid(),
                prior_edge_metadata.gid(),
                prior_edge_metadata.mode()
            )
        );
        assert_eq!(
            std::fs::read_to_string(&world.config.edge_unit_path).unwrap(),
            world.rollback_edge_unit()
        );
        assert!(database_integrity(&world.config.database_path).unwrap());
        assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        for (success, stdout_truncated, stderr_truncated, existing_stderr) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (false, false, false, true),
        ] {
            let world = host_world();
            let output = crate::install::CommandOutput {
                success,
                stdout: "private callback stdout\n".into(),
                stderr: "private callback diagnosis\nprivate cleanup diagnosis\n".into(),
                stdout_truncated,
                stderr_truncated,
            };
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                backup_verification_failure: Some(output.clone()),
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
            let report = world
                .config
                .transaction_dir
                .join("postgres-backup-verified.json");
            if existing_stderr {
                install::ensure_owned_directory(
                    &world.config.transaction_dir,
                    0o700,
                    world.expected_owner,
                )
                .unwrap();
                install::atomic_file(
                    &report.with_extension("stderr"),
                    b"first retained diagnostic\n",
                    0o600,
                    Some(world.expected_owner),
                )
                .unwrap();
            }
            let error = activate(&mut host).unwrap_err();
            assert!(error.contains("PostgreSQL authority verify-backup failed"));
            assert!(error.contains(if existing_stderr {
                "private helper diagnostics incomplete"
            } else {
                "private helper diagnostics retained"
            }));
            assert!(error.contains(if stdout_truncated || stderr_truncated {
                "helper output capture truncated"
            } else {
                "helper output capture complete"
            }));
            assert!(error.contains("without replacing the intact database"));
            assert!(!error.contains("private callback"));
            assert!(!error.contains("private cleanup"));
            assert!(!report.exists());
            let stdout = report.with_extension("stdout");
            let stderr = report.with_extension("stderr");
            let metadata = report.with_extension("command.json");
            assert_eq!(std::fs::read_to_string(&stdout).unwrap(), output.stdout);
            assert_eq!(
                std::fs::read_to_string(&stderr).unwrap(),
                if existing_stderr {
                    "first retained diagnostic\n"
                } else {
                    output.stderr.as_str()
                }
            );
            let value = read_json_file(&metadata).unwrap().unwrap();
            assert_eq!(value["operation"], "verify-backup");
            assert_eq!(value["source_commit"], world.commit);
            assert_eq!(value["success"], success);
            assert_eq!(value["stdout_truncated"], stdout_truncated);
            assert_eq!(value["stderr_truncated"], stderr_truncated);
            assert_eq!(value["stdout_captured_bytes"], output.stdout.len());
            assert_eq!(value["stderr_captured_bytes"], output.stderr.len());
            assert_eq!(value["stdout_retained"], true);
            assert_eq!(value["stderr_retained"], !existing_stderr);
            for path in [&stdout, &stderr, &metadata] {
                let retained = path.symlink_metadata().unwrap();
                assert!(retained.is_file());
                assert_eq!(retained.uid(), world.expected_owner.0);
                assert_eq!(retained.mode() & 0o777, 0o600);
            }
            assert_eq!(
                read_snapshot(&world.config.transaction_dir, world.expected_owner.0)
                    .unwrap()
                    .status,
                "rolled_back"
            );
            assert_eq!(
                std::fs::read_to_string(&world.config.daemon_unit_path).unwrap(),
                world.old_daemon
            );
            assert!(world.config.socket_path.exists());
            assert!(database_integrity(&world.config.database_path).unwrap());
            assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
        }
    }

    #[test]
    fn recovery_refuses_missing_old_and_terminal_targets_before_system_changes() {
        let world = host_world();
        let runner = Arc::new(HostFake {
            commit: world.commit.clone(),
            fragment: world.config.daemon_unit_path.clone(),
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
        host.stop_legacy().unwrap();
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
            source_root: world
                .config
                .candidate_manifest
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("source"),
            edge_env: world.config.edge_env.clone(),
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
        for fault in [
            "dirty_source",
            "stale_source",
            "changed_pin",
            "late_dirty_source",
        ] {
            let world = host_world();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                    .unwrap();
            host.stop_legacy().unwrap();
            let source_root = PathBuf::from(&host.manifest.source_root);
            let pin: PriorEdgeSource = serde_json::from_value(
                finalize::private(
                    &world.config.transaction_dir.join("edge-source-before.json"),
                    world.expected_owner.0,
                )
                .unwrap(),
            )
            .unwrap();
            let pinned_file = pin.source.checkout.join("console/index.html");
            if fault == "changed_pin" {
                std::fs::write(&pinned_file, b"changed pinned source must be retained").unwrap();
            }
            let data_before = install::hash_file(&world.config.database_path).unwrap();
            let static_before = host
                .snapshot_targets()
                .into_iter()
                .map(|path| serde_json::to_value(capture_entry(path).unwrap()).unwrap())
                .collect::<Vec<_>>();
            let recovery_runner = HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                dirty_checkout: fault == "dirty_source",
                dirty_checkout_after_stop: fault == "late_dirty_source",
                upstream_commit: (fault == "stale_source").then(|| "e".repeat(40)),
                ..HostFake::default()
            };
            let config = RecoveryConfig {
                transaction_dir: world.config.transaction_dir.clone(),
                runtime_dir: world.config.runtime_dir.clone(),
                socket_path: world.config.socket_path.clone(),
                database_path: world.config.database_path.clone(),
                systemctl: world.config.systemctl.clone(),
                daemon_unit: world.config.daemon_unit.clone(),
                edge_unit: world.config.edge_unit.clone(),
                edge_state_dir: world.config.edge_state_dir.clone(),
                source_root,
                edge_env: world.config.edge_env.clone(),
            };
            assert!(
                recover_host(&config, &recovery_runner, world.expected_owner).is_err(),
                "{fault}"
            );
            assert_eq!(
                install::hash_file(&world.config.database_path).unwrap(),
                data_before
            );
            let static_after = host
                .snapshot_targets()
                .into_iter()
                .map(|path| serde_json::to_value(capture_entry(path).unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(static_after, static_before, "{fault}");
            for (transaction, unit) in [
                (
                    world.config.transaction_dir.clone(),
                    &world.config.daemon_unit,
                ),
                (
                    startup_guard::edge_transaction(&world.config.transaction_dir),
                    &world.config.edge_unit,
                ),
            ] {
                startup_guard::require_active(
                    &transaction,
                    unit,
                    runner.as_ref(),
                    world.expected_owner.0,
                )
                .unwrap();
            }
            let calls = recovery_runner.requests.lock().unwrap();
            assert!(
                !calls
                    .iter()
                    .any(|request| request.args.first().is_some_and(|arg| arg == "restart")),
                "{fault}"
            );
            if fault != "late_dirty_source" {
                assert!(
                    !calls.iter().any(|request| request.program.file_name()
                        == Some(std::ffi::OsStr::new("systemctl"))
                        || request.program.file_name() == Some(std::ffi::OsStr::new("busctl"))),
                    "source must be checked before recovery changes services"
                );
            }
            if fault == "changed_pin" {
                assert_eq!(
                    std::fs::read(pinned_file).unwrap(),
                    b"changed pinned source must be retained"
                );
            }
        }
        {
            let world = host_world();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                no_prior: true,
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner, world.expected_owner).unwrap();
            std::fs::remove_file(&world.config.daemon_unit_path).unwrap();
            std::fs::remove_file(&world.config.installed_manifest).unwrap();
            host.stop_legacy().unwrap();
            host.backup_database().unwrap();
            let prepared = host.capture_installation().unwrap();
            assert_eq!(prepared.status, "prepared");
            assert!(!prepared.backup_sha256.is_empty());
        }
        // Early recovery preserves static bindings and never accepts an
        // unverified archive. Historical prepared snapshots keep their shape.
        for drift in [false, true] {
            let world = host_world();
            let runner = Arc::new(HostFake {
                commit: world.commit.clone(),
                fragment: world.config.daemon_unit_path.clone(),
                ..HostFake::default()
            });
            let mut host =
                HostCutover::new_owned(world.config.clone(), runner.clone(), world.expected_owner)
                    .unwrap();
            host.stop_legacy().unwrap();
            let frozen =
                read_snapshot(&world.config.transaction_dir, world.expected_owner.0).unwrap();
            assert_eq!(frozen.status, "freezing");
            assert!(frozen.backup_sha256.is_empty());
            startup_guard::require_active(
                &startup_guard::edge_transaction(&world.config.transaction_dir),
                &world.config.edge_unit,
                runner.as_ref(),
                world.expected_owner.0,
            )
            .unwrap();
            let snapshot_path = world
                .config
                .transaction_dir
                .join("installation-snapshot.json");
            let original = std::fs::read(&snapshot_path).unwrap();
            for schema in [2, 3] {
                let mut historical = frozen.clone();
                historical.schema = schema;
                historical.backup_sha256 = "0".repeat(64);
                write_snapshot(&historical, "prepared", world.expected_owner).unwrap();
                let parsed =
                    read_snapshot(&world.config.transaction_dir, world.expected_owner.0).unwrap();
                assert_eq!(parsed.schema, schema);
                assert_eq!(parsed.status, "prepared");
                assert_eq!(
                    serde_json::to_value(parsed.entries).unwrap(),
                    serde_json::to_value(&frozen.entries).unwrap()
                );
            }
            install::atomic_file(&snapshot_path, &original, 0o600, Some(world.expected_owner))
                .unwrap();
            let archive = world
                .config
                .transaction_dir
                .join("authority-before.sqlite3");
            if drift {
                host.backup_database().unwrap();
                std::fs::write(&world.config.instance_env, b"# changed original setting\n")
                    .unwrap();
            } else {
                Connection::open(&world.config.database_path)
                    .unwrap()
                    .backup(rusqlite::MAIN_DB, &archive, None)
                    .unwrap();
            }
            assert!(host.capture_installation().is_err());
            assert_eq!(std::fs::read(&snapshot_path).unwrap(), original);
            startup_guard::require_active(
                &world.config.transaction_dir,
                &world.config.daemon_unit,
                runner.as_ref(),
                world.expected_owner.0,
            )
            .unwrap();
            let recovery = RecoveryConfig {
                transaction_dir: world.config.transaction_dir.clone(),
                runtime_dir: world.config.runtime_dir.clone(),
                socket_path: world.config.socket_path.clone(),
                database_path: world.config.database_path.clone(),
                systemctl: world.config.systemctl.clone(),
                daemon_unit: world.config.daemon_unit.clone(),
                edge_unit: world.config.edge_unit.clone(),
                edge_state_dir: world.config.edge_state_dir.clone(),
                source_root: PathBuf::from(
                    read_json_file(&world.config.candidate_manifest)
                        .unwrap()
                        .unwrap()["source_root"]
                        .as_str()
                        .unwrap(),
                ),
                edge_env: world.config.edge_env.clone(),
            };
            if drift {
                assert!(recover_host(&recovery, runner.as_ref(), world.expected_owner).is_err());
                assert_eq!(
                    std::fs::read(&world.config.instance_env).unwrap(),
                    b"# changed original setting\n"
                );
                startup_guard::require_active(
                    &world.config.transaction_dir,
                    &world.config.daemon_unit,
                    runner.as_ref(),
                    world.expected_owner.0,
                )
                .unwrap();
                startup_guard::require_active(
                    &startup_guard::edge_transaction(&world.config.transaction_dir),
                    &world.config.edge_unit,
                    runner.as_ref(),
                    world.expected_owner.0,
                )
                .unwrap();
            } else {
                Connection::open(&world.config.database_path)
                    .unwrap()
                    .execute(
                        "INSERT INTO meta(key,value) VALUES('early-recovery-live','retained')",
                        [],
                    )
                    .unwrap();
                let recovered =
                    recover_host(&recovery, runner.as_ref(), world.expected_owner).unwrap();
                assert!(!recovered.database_restored);
                for transaction in [
                    world.config.transaction_dir.clone(),
                    startup_guard::edge_transaction(&world.config.transaction_dir),
                ] {
                    assert_eq!(
                        finalize::private(
                            &transaction.join("daemon-start-guard.json"),
                            world.expected_owner.0
                        )
                        .unwrap()["released"],
                        true
                    );
                }
                assert_eq!(
                    Connection::open(&world.config.database_path)
                        .unwrap()
                        .query_row(
                            "SELECT value FROM meta WHERE key='early-recovery-live'",
                            [],
                            |r| r.get::<_, String>(0)
                        )
                        .unwrap(),
                    "retained"
                );
                assert!(
                    !runner
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|r| r.args.iter().any(|a| a == "inspect-postgres"))
                );
            }
        }
        let mut failed_cases = Vec::new();
        for state in ["clean", "initial_dirty", "initial_stale", "late_dirty"] {
            let result = std::panic::catch_unwind(|| {
                let world = host_world();
                let prior_edge_metadata = std::fs::metadata(&world.config.edge_env).unwrap();
                let runner = Arc::new(HostFake {
                    commit: world.commit.clone(),
                    fragment: world.config.daemon_unit_path.clone(),
                    ..HostFake::default()
                });
                let mut host = HostCutover::new_owned(
                    world.config.clone(),
                    runner.clone(),
                    world.expected_owner,
                )
                .unwrap();
                let drain = host.close_admission().unwrap();
                host.fence_legacy_socket().unwrap();
                host.wait_for_quiescence(&drain).unwrap();
                host.stop_legacy().unwrap();
                host.backup_database().unwrap();
                host.capture_installation().unwrap();
                host.install_rust().unwrap();
                assert_eq!(
                    std::fs::read_to_string(&world.config.edge_env).unwrap(),
                    world.old_edge_env.replace(&"b".repeat(40), &world.commit)
                );
                std::fs::write(&world.config.database_path, b"not a database").unwrap();
                std::fs::write(
                    world.config.runtime_dir.join(DRAIN_FILE),
                    b"{\"schema\":1,\"pid\":4294967295,\"process_start\":\"gone\",\"nonce\":\"stale\"}",
                )
                .unwrap();
                let source_root = PathBuf::from(&host.manifest.source_root);
                let candidate_entries = host
                    .snapshot_targets()
                    .into_iter()
                    .map(capture_entry)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let snapshot_path = world
                    .config
                    .transaction_dir
                    .join("installation-snapshot.json");
                let snapshot_before = std::fs::read(&snapshot_path).unwrap();
                let drain_before =
                    std::fs::read(world.config.runtime_dir.join(DRAIN_FILE)).unwrap();
                let lock_before = capture_entry(&world.config.runtime_dir.join(LOCK_FILE)).unwrap();
                drop(host);

                let recovery_runner = HostFake {
                    commit: world.commit.clone(),
                    fragment: world.config.daemon_unit_path.clone(),
                    upstream_commit: (state == "initial_stale").then(|| "e".repeat(40)),
                    dirty_checkout: state == "initial_dirty",
                    dirty_checkout_after_stop: state == "late_dirty",
                    ..HostFake::default()
                };
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
                        source_root,
                        edge_env: world.config.edge_env.clone(),
                    },
                    &recovery_runner,
                    world.expected_owner,
                );
                if state.starts_with("initial_") {
                    let error = recovery.unwrap_err();
                    assert!(
                        error.contains("source binding is unverified"),
                        "{state}: {error}"
                    );
                    assert!(!error.contains("marker was cleared"));
                    assert!(!error.contains("retained-private-setting"));
                    assert!(
                        !recovery_runner
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|request| { request.program == world.config.systemctl }),
                        "{state}: recovery must not stop, restart, or reload services"
                    );
                    let after_entries = candidate_entries
                        .iter()
                        .map(|entry| capture_entry(Path::new(&entry.path)))
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(&after_entries).unwrap(),
                        serde_json::to_value(&candidate_entries).unwrap()
                    );
                    assert_eq!(
                        std::fs::read(&world.config.database_path).unwrap(),
                        b"not a database"
                    );
                    assert_eq!(std::fs::read(&snapshot_path).unwrap(), snapshot_before);
                    assert_eq!(
                        std::fs::read(world.config.runtime_dir.join(DRAIN_FILE)).unwrap(),
                        drain_before
                    );
                    assert_eq!(
                        serde_json::to_value(
                            capture_entry(&world.config.runtime_dir.join(LOCK_FILE)).unwrap()
                        )
                        .unwrap(),
                        serde_json::to_value(&lock_before).unwrap()
                    );
                    return;
                }
                let expected_env = if state == "late_dirty" {
                    let error = recovery.unwrap_err();
                    assert!(error.contains("source binding is unverified"));
                    assert!(!error.contains("retained-private-setting"));
                    assert!(
                        !recovery_runner
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|request| {
                                request.args.first() == Some(&OsString::from("restart"))
                            })
                    );
                    world.old_edge_env.clone()
                } else {
                    let receipt = recovery.unwrap();
                    assert_eq!(receipt.status, "recovered");
                    assert!(receipt.database_restored);
                    assert!(world.config.socket_path.exists());
                    assert!(!world.config.runtime_dir.join(DRAIN_FILE).exists());
                    assert!(
                        !serde_json::to_string(&receipt)
                            .unwrap()
                            .contains("retained-private-setting")
                    );
                    world.rollback_edge_env()
                };
                assert_eq!(
                    std::fs::read_to_string(&world.config.edge_env).unwrap(),
                    expected_env
                );
                let edge_metadata = std::fs::metadata(&world.config.edge_env).unwrap();
                assert_eq!(
                    (
                        edge_metadata.uid(),
                        edge_metadata.gid(),
                        edge_metadata.mode()
                    ),
                    (
                        prior_edge_metadata.uid(),
                        prior_edge_metadata.gid(),
                        prior_edge_metadata.mode()
                    )
                );
                assert_eq!(
                    std::fs::read_to_string(&world.config.edge_unit_path).unwrap(),
                    if state == "late_dirty" {
                        world.old_edge_unit.clone()
                    } else {
                        world.rollback_edge_unit()
                    }
                );
                assert_eq!(
                    std::fs::read_to_string(&world.config.daemon_unit_path).unwrap(),
                    world.old_daemon
                );
                assert_eq!(
                    std::fs::read_to_string(&world.config.cli_link).unwrap(),
                    world.old_cli
                );
                assert!(database_integrity(&world.config.database_path).unwrap());
                let prior_manifest: Value = serde_json::from_slice(
                    &std::fs::read(&world.config.installed_manifest).unwrap(),
                )
                .unwrap();
                assert_eq!(prior_manifest["source_commit"], "b".repeat(40));
                let snapshot =
                    read_snapshot(&world.config.transaction_dir, world.expected_owner.0).unwrap();
                assert_eq!(snapshot.status == "recovered", state == "clean");
                let saved_edge = snapshot
                    .entries
                    .iter()
                    .find(|entry| Path::new(&entry.path) == world.config.edge_env)
                    .unwrap();
                assert_eq!(
                    BASE64
                        .decode(saved_edge.content_base64.as_ref().unwrap())
                        .unwrap(),
                    world.old_edge_env.as_bytes()
                );
            });
            if result.is_err() {
                failed_cases.push(state);
            }
        }
        assert!(
            failed_cases.is_empty(),
            "failed recovery states: {failed_cases:?}"
        );
    }
}
