//! The dedicated PostgreSQL service is prepared only by the reviewed installer.
use super::{
    CommandOutput, CommandRequest, CommandRunner, InstallManifest, atomic_file,
    ensure_owned_directory, path_text, read_template, validate_account_name, validate_regular,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const ACCEPTANCE_CHECKS: &[&str] = &[
    "workspace",
    "skills",
    "root-acceptance",
    "postgres-migration",
    "postgres-restart",
    "critical-api",
    "functional",
];

#[derive(Clone, Debug)]
pub struct PostgresSetup {
    pub account: String,
    pub database_name: String,
    pub bin_dir: PathBuf,
    pub data_dir: PathBuf,
    pub socket_dir: PathBuf,
    pub config_path: PathBuf,
    pub unit_path: PathBuf,
    pub unit: String,
}
impl Default for PostgresSetup {
    fn default() -> Self {
        Self {
            account: "devcoordinator2-pg".into(),
            database_name: "devcoordinator2".into(),
            bin_dir: "/usr/lib/postgresql/17/bin".into(),
            data_dir: "/var/lib/devcoordinator2-postgres/data".into(),
            socket_dir: "/run/devcoordinator2-postgres".into(),
            config_path: "/etc/devcoordinator2/postgres.json".into(),
            unit_path: "/etc/systemd/system/devcoordinator2-postgres.service".into(),
            unit: "devcoordinator2-postgres.service".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateAcceptance {
    pub schema: u8,
    pub result: String,
    pub source_commit: String,
    pub binaries: BTreeMap<String, String>,
    pub checks: Vec<String>,
}

pub fn verify_acceptance(
    path: &Path,
    manifest: &InstallManifest,
    owner: u32,
) -> Result<String, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "a private full-candidate acceptance receipt is required")?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect candidate acceptance receipt")?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() > 1024 * 1024
    {
        return Err("candidate acceptance receipt must be an owned mode-0600 regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read candidate acceptance receipt")?;
    let receipt: CandidateAcceptance =
        serde_json::from_slice(&bytes).map_err(|_| "candidate acceptance receipt is invalid")?;
    let checks = receipt
        .checks
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if receipt.schema != 1
        || receipt.result != "passed"
        || receipt.source_commit != manifest.source_commit
        || receipt.binaries.len() != manifest.binaries.len()
        || manifest
            .binaries
            .iter()
            .any(|b| receipt.binaries.get(&b.name) != Some(&b.sha256))
        || ACCEPTANCE_CHECKS
            .iter()
            .any(|check| !checks.contains(check))
    {
        return Err("candidate acceptance does not prove the exact verified binaries and every required check".into());
    }
    use sha2::Digest;
    Ok(super::hex(&sha2::Sha256::digest(&bytes)))
}
use std::os::unix::fs::OpenOptionsExt;

impl PostgresSetup {
    fn command(
        &self,
        runner: &dyn CommandRunner,
        program: &Path,
        args: Vec<OsString>,
    ) -> Result<CommandOutput, String> {
        runner.run(&CommandRequest {
            program: program.to_owned(),
            args,
            environment: BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/sbin:/usr/bin:/sbin:/bin"),
            )]),
            clear_environment: true,
        })
    }
    fn required(
        &self,
        runner: &dyn CommandRunner,
        program: &Path,
        args: Vec<OsString>,
        step: &str,
    ) -> Result<CommandOutput, String> {
        let output = self.command(runner, program, args)?;
        if !output.success || output.stdout_truncated || output.stderr_truncated {
            return Err(format!(
                "dedicated PostgreSQL {step} failed; private command evidence is required"
            ));
        }
        Ok(output)
    }
    fn identity(&self, runner: &dyn CommandRunner) -> Result<(u32, u32), String> {
        validate_account_name(&self.account, "PostgreSQL service account")?;
        let inspect = |flag: &str| {
            self.command(
                runner,
                Path::new("/usr/bin/id"),
                vec![flag.into(), self.account.clone().into()],
            )
        };
        let mut uid = inspect("-u")?;
        if !uid.success {
            self.required(
                runner,
                Path::new("/usr/sbin/useradd"),
                vec![
                    "--system".into(),
                    "--no-create-home".into(),
                    "--user-group".into(),
                    "--shell".into(),
                    "/usr/sbin/nologin".into(),
                    "--home-dir".into(),
                    self.data_dir
                        .parent()
                        .ok_or("PostgreSQL data directory has no parent")?
                        .as_os_str()
                        .to_owned(),
                    self.account.clone().into(),
                ],
                "identity creation",
            )?;
            uid = inspect("-u")?;
        }
        let gid = inspect("-g")?;
        let parse = |output: CommandOutput| -> Result<u32, String> {
            if !output.success || output.stdout_truncated {
                return Err("cannot verify dedicated PostgreSQL identity".into());
            }
            output
                .stdout
                .trim()
                .parse()
                .map_err(|_| "cannot verify dedicated PostgreSQL identity".into())
        };
        let identity = (parse(uid)?, parse(gid)?);
        if identity.0 == 0 || identity.1 == 0 {
            return Err("PostgreSQL must use a dedicated non-root identity".into());
        }
        Ok(identity)
    }
    pub fn render_unit(&self, source: &Path) -> Result<String, String> {
        let template = read_template(&source.join("deploy/devcoordinator2-postgres.service"))?;
        for path in [
            &self.bin_dir,
            &self.data_dir,
            &self.socket_dir,
            &self.config_path,
            &self.unit_path,
        ] {
            if !path.is_absolute()
                || path.as_os_str().as_encoded_bytes().iter().any(|b| {
                    b.is_ascii_whitespace() || matches!(*b, b'\n' | b'\r' | b'%' | b'\\' | b'\'')
                })
            {
                return Err(
                    "PostgreSQL installation paths must be absolute and safe unit arguments".into(),
                );
            }
        }
        let rendered = template
            .replace("User=devcoordinator2-pg", &format!("User={}", self.account))
            .replace(
                "Group=devcoordinator2-pg",
                &format!("Group={}", self.account),
            )
            .replace("/usr/lib/postgresql/17/bin", &path_text(&self.bin_dir)?)
            .replace(
                "/var/lib/devcoordinator2-postgres/data",
                &path_text(&self.data_dir)?,
            )
            .replace(
                "/run/devcoordinator2-postgres",
                &path_text(&self.socket_dir)?,
            );
        let rendered = rendered.replace(
            "ReadWritePaths=/var/lib/devcoordinator2-postgres ",
            &format!(
                "ReadWritePaths={} ",
                self.data_dir
                    .parent()
                    .ok_or("PostgreSQL data directory has no parent")?
                    .display()
            ),
        );
        let rendered = if self.data_dir == Path::new("/var/lib/devcoordinator2-postgres/data") {
            rendered
        } else {
            rendered.replace(
                "StateDirectory=devcoordinator2-postgres\nStateDirectoryMode=0700\n",
                "",
            )
        };
        let rendered = if self.socket_dir == Path::new("/run/devcoordinator2-postgres") {
            rendered
        } else {
            rendered.replace(
                "RuntimeDirectory=devcoordinator2-postgres\nRuntimeDirectoryMode=0700\n",
                "",
            )
        };
        Ok(rendered)
    }
    pub fn verify_binaries(
        &self,
        runner: &dyn CommandRunner,
        owner: (u32, u32),
    ) -> Result<(), String> {
        for name in [
            "postgres",
            "initdb",
            "psql",
            "pg_dump",
            "pg_restore",
            "pg_isready",
        ] {
            let metadata = validate_regular(&self.bin_dir.join(name), true)
                .map_err(|_| format!("reviewed PostgreSQL 17 {name} executable is required before cutover; provision the permanent PostgreSQL 17 dependency without creating a default cluster"))?;
            if metadata.uid() != owner.0 || metadata.mode() & 0o022 != 0 {
                return Err("PostgreSQL executables must be owned by the installation owner and not group/world writable".into());
            }
            let version = self.required(
                runner,
                &self.bin_dir.join(name),
                vec!["--version".into()],
                "version check",
            )?;
            if !version
                .stdout
                .trim()
                .starts_with(&format!("{name} (PostgreSQL) 17."))
            {
                return Err(
                    "every dedicated authority executable requires PostgreSQL major version 17"
                        .into(),
                );
            }
        }
        Ok(())
    }
    pub fn prepare(
        &self,
        source: &Path,
        runner: &dyn CommandRunner,
        owner: (u32, u32),
    ) -> Result<(), String> {
        self.verify_binaries(runner, owner)?;
        let identity = self.identity(runner)?;
        // Refuse unknown data before changing any existing directory owner.
        // A recognizable cluster must retain the installer-owned binding.
        let parent = self
            .data_dir
            .parent()
            .ok_or("PostgreSQL data directory has no parent")?;
        let marker = parent.join("authority-install.json");
        let expected_marker = serde_json::json!({"schema":1,"product":"devcoordinator2","major":17,"data":self.data_dir,"socket":self.socket_dir,"unit":self.unit});
        if let Ok(metadata) = self.data_dir.symlink_metadata() {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("existing PostgreSQL data target is not a regular directory".into());
            }
            if self.data_dir.join("PG_VERSION").exists() {
                let proof = marker
                    .symlink_metadata()
                    .map_err(|_| "existing PostgreSQL data has no owned installation binding")?;
                if proof.file_type().is_symlink()
                    || !proof.is_file()
                    || proof.uid() != owner.0
                    || proof.mode() & 0o777 != 0o600
                    || proof.len() > 65536
                    || metadata.uid() != identity.0
                    || serde_json::from_slice::<serde_json::Value>(
                        &std::fs::read(&marker)
                            .map_err(|_| "cannot inspect PostgreSQL ownership binding")?,
                    )
                    .map_err(|_| "PostgreSQL ownership binding is invalid")?
                        != expected_marker
                    || std::fs::read_to_string(self.data_dir.join("PG_VERSION"))
                        .map_err(|_| "cannot inspect PostgreSQL major version")?
                        .trim()
                        != "17"
                {
                    return Err("existing PostgreSQL data ownership is unverified".into());
                }
            } else if std::fs::read_dir(&self.data_dir)
                .map_err(|_| "cannot inspect existing PostgreSQL target")?
                .next()
                .is_some()
            {
                return Err(
                    "PostgreSQL initialization will not replace nonempty unverified data".into(),
                );
            }
        }
        if parent.exists() && !marker.exists() {
            for entry in
                std::fs::read_dir(parent).map_err(|_| "cannot inspect PostgreSQL state parent")?
            {
                if entry
                    .map_err(|_| "cannot inspect PostgreSQL state parent")?
                    .path()
                    != self.data_dir
                {
                    return Err("PostgreSQL state parent contains unverified data".into());
                }
            }
        }
        ensure_owned_directory(
            self.data_dir
                .parent()
                .ok_or("PostgreSQL data directory has no parent")?,
            0o700,
            identity,
        )?;
        ensure_owned_directory(&self.data_dir, 0o700, identity)?;
        ensure_owned_directory(&self.socket_dir, 0o700, identity)?;
        let initialized = self.data_dir.join("PG_VERSION");
        let marker = self
            .data_dir
            .parent()
            .ok_or("PostgreSQL data directory has no parent")?
            .join("authority-install.json");
        let marker_value = serde_json::json!({"schema":1,"product":"devcoordinator2","major":17,"data":self.data_dir,"socket":self.socket_dir,"unit":self.unit});
        if initialized.exists() {
            let metadata = marker.symlink_metadata().map_err(
                |_| "an existing PostgreSQL cluster is not an owned Coordinator authority",
            )?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != owner.0
                || metadata.mode() & 0o777 != 0o600
                || serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(&marker)
                        .map_err(|_| "cannot verify dedicated PostgreSQL ownership")?,
                )
                .map_err(|_| "invalid dedicated PostgreSQL ownership")?
                    != marker_value
            {
                return Err(
                    "existing PostgreSQL cluster ownership does not match this installation".into(),
                );
            }
            let metadata = initialized
                .symlink_metadata()
                .map_err(|_| "cannot inspect dedicated PostgreSQL cluster")?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || std::fs::read_to_string(&initialized)
                    .map_err(|_| "cannot inspect dedicated PostgreSQL version")?
                    .trim()
                    != "17"
            {
                return Err(
                    "existing dedicated PostgreSQL cluster has an unexpected identity or version"
                        .into(),
                );
            }
        } else {
            if std::fs::read_dir(&self.data_dir)
                .map_err(|_| "cannot inspect dedicated PostgreSQL directory")?
                .next()
                .is_some()
            {
                return Err(
                    "PostgreSQL initialization requires an empty dedicated data directory".into(),
                );
            }
            self.required(
                runner,
                Path::new("/usr/bin/setpriv"),
                vec![
                    format!("--reuid={}", identity.0).into(),
                    format!("--regid={}", identity.1).into(),
                    "--clear-groups".into(),
                    "--".into(),
                    self.bin_dir.join("initdb").into_os_string(),
                    "--pgdata".into(),
                    self.data_dir.clone().into_os_string(),
                    "--username=devcoordinator2_admin".into(),
                    "--auth-local=peer".into(),
                    "--auth-host=reject".into(),
                    "--encoding=UTF8".into(),
                    "--locale=C".into(),
                ],
                "cluster initialization",
            )?;
        }
        atomic_file(
            &marker,
            &serde_json::to_vec(&marker_value).map_err(|_| "cannot encode PostgreSQL ownership")?,
            0o600,
            Some(owner),
        )?;
        let settings = format!(
            "listen_addresses = ''\nunix_socket_directories = '{}'\nunix_socket_permissions = 0700\nmax_connections = 48\nshared_buffers = '64MB'\npassword_encryption = 'scram-sha-256'\nlog_statement = 'none'\nlog_min_error_statement = 'panic'\nlog_error_verbosity = 'terse'\nlog_parameter_max_length = 0\nlog_parameter_max_length_on_error = 0\nlogging_collector = on\nlog_destination = 'stderr'\nlog_directory = 'log'\nlog_file_mode = 0600\nlog_rotation_age = '1d'\nlog_rotation_size = '16MB'\n",
            self.socket_dir.display()
        );
        atomic_file(
            &self.data_dir.join("postgresql.conf"),
            settings.as_bytes(),
            0o600,
            Some(identity),
        )?;
        atomic_file(&self.data_dir.join("pg_hba.conf"),b"local all devcoordinator2_admin peer map=devcoordinator2\nlocal all devcoordinator2 peer map=devcoordinator2\nlocal all all reject\nhost all all 0.0.0.0/0 reject\nhost all all ::/0 reject\n",0o600,Some(identity))?;
        let mapping = format!(
            "devcoordinator2 root devcoordinator2_admin\ndevcoordinator2 root devcoordinator2\ndevcoordinator2 {} devcoordinator2_admin\n",
            self.account
        );
        atomic_file(
            &self.data_dir.join("pg_ident.conf"),
            mapping.as_bytes(),
            0o600,
            Some(identity),
        )?;
        atomic_file(
            &self.unit_path,
            self.render_unit(source)?
                .replace(
                    &format!("Group={}", self.account),
                    &format!("Group={}", identity.1),
                )
                .as_bytes(),
            0o644,
            Some(owner),
        )?;
        // Peer authentication keeps credentials and connection parameters out
        // of argv and environment values. Thin clients never read this file.
        self.validate_database_name()?;
        let connection = format!(
            "host={} user=devcoordinator2 dbname={}",
            self.socket_dir.display(),
            self.database_name
        );
        let private = serde_json::to_vec_pretty(
            &serde_json::json!({"schema":1,"connection":connection,"data_directory":self.data_dir}),
        )
        .map_err(|_| "cannot encode private PostgreSQL configuration")?;
        if let Some(parent) = self.config_path.parent() {
            if parent.exists() {
                let metadata = parent
                    .symlink_metadata()
                    .map_err(|_| "cannot inspect private configuration directory")?;
                if metadata.file_type().is_symlink()
                    || !metadata.is_dir()
                    || metadata.uid() != owner.0
                    || metadata.mode() & 0o002 != 0
                {
                    return Err(
                        "private configuration directory has an unexpected owner or write boundary"
                            .into(),
                    );
                }
            } else {
                ensure_owned_directory(parent, 0o750, owner)?;
            }
        }
        if self.config_path.exists() {
            let metadata = self
                .config_path
                .symlink_metadata()
                .map_err(|_| "cannot inspect private PostgreSQL configuration")?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != owner.0
                || metadata.mode() & 0o777 != 0o600
            {
                return Err("existing PostgreSQL configuration is not private and owned".into());
            }
            let current: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&self.config_path)
                    .map_err(|_| "cannot read private PostgreSQL configuration")?,
            )
            .map_err(|_| "invalid existing PostgreSQL configuration")?;
            if current.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
                || current
                    .get("connection")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                return Err("existing private PostgreSQL configuration is invalid".into());
            }
        }
        atomic_file(&self.config_path, &private, 0o600, Some(owner))?;
        Ok(())
    }
    pub fn wait_ready(&self, runner: &dyn CommandRunner) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut delay = std::time::Duration::from_millis(25);
        loop {
            let output = self.command(
                runner,
                &self.bin_dir.join("pg_isready"),
                vec![
                    "--host".into(),
                    self.socket_dir.clone().into_os_string(),
                    "--username=devcoordinator2_admin".into(),
                    "--dbname=postgres".into(),
                    "--timeout=1".into(),
                ],
            )?;
            if output.success {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("dedicated PostgreSQL Unix socket did not become ready before the activation deadline".into());
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(std::time::Duration::from_millis(500));
        }
    }

    fn validate_database_name(&self) -> Result<(), String> {
        if self.database_name.len() > 63
            || (self.database_name != "devcoordinator2"
                && !self.database_name.starts_with("devcoordinator2_"))
            || !self
                .database_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(
                "candidate database name must have an exact generated Coordinator identity".into(),
            );
        }
        Ok(())
    }

    pub fn bootstrap(&self, runner: &dyn CommandRunner) -> Result<(), String> {
        self.wait_ready(runner)?;
        self.validate_database_name()?;
        let common = vec![
            "--no-psqlrc".into(),
            "--no-password".into(),
            "--host".into(),
            self.socket_dir.clone().into_os_string(),
            "--username=devcoordinator2_admin".into(),
            "--dbname=postgres".into(),
            "--set=ON_ERROR_STOP=1".into(),
        ];
        let mut roles = common.clone();
        roles.extend(["--command".into(),"DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='devcoordinator2') THEN CREATE ROLE devcoordinator2 LOGIN CREATEDB NOSUPERUSER NOCREATEROLE NOREPLICATION; END IF; END $$;".into()]);
        self.required(
            runner,
            &self.bin_dir.join("psql"),
            roles,
            "authority role preparation",
        )?;
        let mut inspect = common.clone();
        inspect.extend([
            "--tuples-only".into(),
            "--no-align".into(),
            "--command".into(),
            format!(
                "SELECT 1 FROM pg_database WHERE datname='{}'",
                self.database_name
            )
            .into(),
        ]);
        let exists = self.required(
            runner,
            &self.bin_dir.join("psql"),
            inspect,
            "authority database inspection",
        )?;
        if exists.stdout.trim().is_empty() {
            let mut create = common;
            create.extend(["--command".into(),format!("CREATE DATABASE \"{}\" OWNER devcoordinator2 TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C';",self.database_name).into()]);
            self.required(
                runner,
                &self.bin_dir.join("psql"),
                create,
                "authority database creation",
            )?;
        }
        Ok(())
    }
}
