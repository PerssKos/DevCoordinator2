use super::*;
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[path = "host/startup_guard.rs"]
mod guard_scenario;

pub(super) fn hold_startup_guard(work_root: &Path, case: &str) -> Result<(), String> {
    guard_scenario::hold(work_root, case)
}
pub(super) fn recover_startup_guard(work_root: &Path, case: &str) -> Result<Value, String> {
    guard_scenario::recover(work_root, case)
}

pub(super) fn run_startup_guard(config: GuardConfig) -> Result<Value, String> {
    guard_scenario::run(config)
}

pub(super) struct Fixture {
    pub marker: Marker,
    input: Config,
    layout: HostCutoverConfig,
    candidate: InstallManifest,
    final_candidate: Option<InstallManifest>,
    prior: InstallManifest,
    artifacts: PathBuf,
    runner: Arc<Recorder>,
    repository_id: String,
    runtime_prepared: bool,
}

struct Recorder {
    directory: PathBuf,
    next: Mutex<u32>,
    owner: (u32, u32),
}

#[derive(PartialEq, Eq)]
struct NativeState {
    database_oid: u64,
    migration: u64,
    postgres_config_sha256: String,
    authority_marker_sha256: String,
    installed_manifest_sha256: String,
    instance_env_sha256: String,
    edge_env_sha256: String,
    edge_unit_sha256: String,
    retirement_targets: Value,
}
impl Recorder {
    fn retain(
        &self,
        request: &CommandRequest,
        result: &Result<install::CommandOutput, String>,
    ) -> Result<(), String> {
        let mut next = self
            .next
            .lock()
            .map_err(|_| "fixture command catalog is unavailable")?;
        *next += 1;
        let stem = format!("command-{:05}", *next);
        let metadata = serde_json::json!({"schema":1,"program":request.program,"args":request.args,"success":result.as_ref().is_ok_and(|output|output.success),"error":result.as_ref().err(),"stdout_truncated":result.as_ref().ok().map(|output|output.stdout_truncated),"stderr_truncated":result.as_ref().ok().map(|output|output.stderr_truncated)});
        install::atomic_file(
            &self.directory.join(format!("{stem}.json")),
            &serde_json::to_vec(&metadata).map_err(|_| "cannot encode command evidence")?,
            0o600,
            Some(self.owner),
        )?;
        if let Ok(output) = result {
            for (stream, text) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
                install::atomic_file(
                    &self.directory.join(format!("{stem}.{stream}")),
                    text.as_bytes(),
                    0o600,
                    Some(self.owner),
                )?;
            }
        }
        Ok(())
    }
}
impl CommandRunner for Recorder {
    fn run(&self, request: &CommandRequest) -> Result<install::CommandOutput, String> {
        let result = install::HostRunner.run(request);
        self.retain(request, &result)?;
        result
    }
    fn source_catalog(&self, request: &CommandRequest) -> Result<install::CommandOutput, String> {
        let result = install::HostRunner.source_catalog(request);
        self.retain(request, &result)?;
        result
    }
}

fn directory(path: &Path, mode: u32, owner: (u32, u32)) -> Result<(), String> {
    install::ensure_owned_directory(path, mode, owner)
}
fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    install::atomic_file(path, bytes, mode, Some((0, 0)))
}
fn execute(
    runner: &dyn CommandRunner,
    program: &Path,
    args: Vec<OsString>,
) -> Result<install::CommandOutput, String> {
    runner.run(&CommandRequest {
        program: program.to_owned(),
        args,
        environment: BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        )]),
        clear_environment: true,
    })
}
fn required(
    runner: &dyn CommandRunner,
    program: &Path,
    args: Vec<OsString>,
) -> Result<install::CommandOutput, String> {
    let output = execute(runner, program, args)?;
    if !output.success || output.stdout_truncated || output.stderr_truncated {
        return Err("isolated installer command failed; bounded private evidence retained".into());
    }
    Ok(output)
}
fn binary(manifest: &InstallManifest, name: &str) -> Result<PathBuf, String> {
    manifest
        .binaries
        .iter()
        .find(|binary| binary.name == name)
        .map(|binary| PathBuf::from(&binary.path))
        .ok_or_else(|| format!("explicit fixture {name} binary is unavailable"))
}

fn fixture_unit_available(name: &str, output: &install::CommandOutput) -> bool {
    if output.stdout_truncated || output.stderr_truncated {
        return false;
    }
    let mut properties = BTreeMap::new();
    for line in output.stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        if properties.insert(key, value).is_some() {
            return false;
        }
    }
    let value = |key: &str| properties.get(key).copied();
    properties.len() == 7
        && value("ActiveState") == Some("inactive")
        && value("FragmentPath") == Some("")
        && value("SourcePath") == Some("")
        && value("DropInPaths") == Some("")
        && value("ControlGroup") == Some("")
        && value("Transient") == Some("no")
        && (value("LoadState") == Some("not-found")
            || (name.ends_with(".slice") && output.success && value("LoadState") == Some("loaded")))
}

fn release_fixture_fifo(owned: &Marker, deadline: Instant) -> Result<(), String> {
    if marker(&owned.root, 0)? != *owned {
        return Err("fixture FIFO ownership changed; release refused".into());
    }
    let path = owned.root.join("release.fifo");
    let before = path
        .symlink_metadata()
        .map_err(|_| "fixture release FIFO is unavailable")?;
    if !before.file_type().is_fifo() || before.uid() != 0 {
        return Err("fixture release path is not its owned FIFO".into());
    }
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(mut writer) => {
                let current = writer
                    .metadata()
                    .map_err(|_| "fixture release identity is unavailable")?;
                if !current.file_type().is_fifo()
                    || (current.dev(), current.ino()) != (before.dev(), before.ino())
                {
                    return Err("fixture release FIFO identity changed".into());
                }
                return writer
                    .write_all(b"fixture release\n")
                    .map_err(|_| "accepted fixture release failed".into());
            }
            Err(error)
                if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Err("accepted fixture did not reach its bounded release".into()),
        }
    }
}

fn client_as(
    helper: &Path,
    socket: &Path,
    uid: u32,
    gid: u32,
    value: &Value,
) -> Result<Value, String> {
    let mut child = Command::new("/usr/bin/setpriv")
        .args([
            format!("--reuid={uid}"),
            format!("--regid={gid}"),
            "--clear-groups".into(),
            "--".into(),
        ])
        .arg(helper)
        .args(["request", "--socket"])
        .arg(socket)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "cannot start the physical fixture caller")?;
    {
        let mut input = child
            .stdin
            .take()
            .ok_or("fixture client input is unavailable")?;
        input
            .write_all(
                &serde_json::to_vec(value).map_err(|_| "cannot encode fixture client input")?,
            )
            .map_err(|_| "fixture client input failed")?;
    }
    let output = child
        .wait_with_output()
        .map_err(|_| "fixture caller failed")?;
    if !output.status.success() || output.stdout.len() > 2 * 1024 * 1024 {
        return Err("physical fixture caller did not return a bounded response".into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "physical fixture caller returned invalid JSON".into())
}

impl Fixture {
    pub fn prepare(
        input: &Config,
        candidate: &InstallManifest,
        prior: &InstallManifest,
        native_prior: Option<&InstallManifest>,
    ) -> Result<Self, String> {
        if input.work_root.parent() != Some(Path::new("/var/lib"))
            || !input
                .work_root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(PREFIX))
            || input.work_root.symlink_metadata().is_ok()
            || input.report.starts_with(&input.work_root)
        {
            return Err("rehearsal requires a new /var/lib/devcoordinator2-rustint-installer-* root and external report".into());
        }
        let pg = install::account_by_name(&input.postgres_account)?;
        if pg.uid == 0 || pg.gid == 0 {
            return Err("fixture PostgreSQL account must be non-root".into());
        }
        let edge = install::account_by_name("devcoordinator2-edge")?;
        let artifacts = input.report.with_extension("artifacts");
        if artifacts.symlink_metadata().is_ok()
            || artifacts.starts_with(&input.work_root)
            || input.work_root.starts_with(&artifacts)
        {
            return Err("fixture evidence directory must be new".into());
        }
        directory(&artifacts, 0o700, (0, 0))?;
        directory(&input.work_root, 0o711, (0, 0))?;
        let namespace = format!("{PREFIX}{}-{}", std::process::id(), random_hex(4)?);
        let marker = Marker {
            schema: 1,
            kind: KIND.into(),
            fixture_only: true,
            root: input.work_root.clone(),
            namespace: namespace.clone(),
            source_commit: candidate.source_commit.clone(),
            native_binding_sha256: None,
        };
        write(
            &input.work_root.join("ownership.json"),
            &serde_json::to_vec(&marker).map_err(|_| "cannot retain fixture ownership")?,
            0o600,
        )?;
        let root = &input.work_root;
        let unit = |suffix: &str| format!("{namespace}-{suffix}");
        let unit_path = |suffix: &str| Path::new("/run/systemd/system").join(unit(suffix));
        let layout = HostCutoverConfig {
            candidate_acceptance: root.join("inputs.json"),
            candidate_manifest: root.join("candidate.json"),
            installed_manifest: root.join("etc/install-manifest.json"),
            source_cache_dir: root.join("sources"),
            tmpfiles_path: root.join("etc/tmpfiles.conf"),
            transaction_dir: root.join("cutover/rollback"),
            runtime_dir: root.join("run"),
            socket_path: root.join("run/daemon.sock"),
            database_path: root.join("state/authority.sqlite3"),
            daemon_unit_path: unit_path("daemon.service"),
            edge_unit_path: unit_path("edge.service"),
            edge_env: root.join("etc/edge.env"),
            tests_slice_unit_path: unit_path("tests.slice"),
            cli_link: root.join("bin/devcoordinator2"),
            tooling_link: root.join("bin/devcoordinator2-tooling"),
            systemctl: "/usr/bin/systemctl".into(),
            ss: "/usr/bin/ss".into(),
            instance_env: root.join("etc/instance.env"),
            daemon_unit: unit("daemon.service"),
            edge_unit: unit("edge.service"),
            edge_state_dir: root.join("edge-state"),
            canary: true,
            ephemeral_services: true,
            postgres: install::postgres::PostgresSetup {
                account: input.postgres_account.clone(),
                database_name: "devcoordinator2_rehearsal".into(),
                bin_dir: input.postgres_bin.clone(),
                data_dir: root.join("postgres/data"),
                socket_dir: root.join("run/postgres"),
                config_path: root.join("etc/postgres.json"),
                unit_path: unit_path("postgres.service"),
                unit: unit("postgres.service"),
            },
        };
        let mut fixture = Self {
            marker,
            input: input.clone(),
            layout,
            candidate: candidate.clone(),
            final_candidate: None,
            prior: prior.clone(),
            artifacts: artifacts.clone(),
            runner: Arc::new(Recorder {
                directory: artifacts,
                next: Mutex::new(0),
                owner: (0, 0),
            }),
            repository_id: String::new(),
            runtime_prepared: false,
        };
        let initialized = (|| {
            if let Some(native) = native_prior {
                fixture.prepare_native_source(native)?;
            }
            fixture.initialize(&edge)
        })();
        if let Err(error) = initialized {
            let cleanup = fixture.cleanup();
            let report = Report {
                schema: 1,
                fixture_only: true,
                status: "failed".into(),
                source_commit: candidate.source_commit.clone(),
                prior_source_commit: prior.source_commit.clone(),
                native_prior_source_commit: native_prior.map(|prior| prior.source_commit.clone()),
                namespace: fixture.marker.namespace.clone(),
                checks: Vec::new(),
                cleanup_complete: cleanup.is_ok(),
                error: Some(format!(
                    "fixture preparation failed: {error}{}",
                    cleanup
                        .as_ref()
                        .err()
                        .map(|error| format!("; cleanup: {error}"))
                        .unwrap_or_default()
                )),
                report_path: input.report.clone(),
            };
            if retain_report(&report).is_err() {
                return Err(format!(
                    "fixture preparation failed: {error}; cleanup verified: {}; failed report could not be retained",
                    cleanup.is_ok()
                ));
            }
            return Err(format!(
                "fixture preparation failed: {error}; cleanup verified: {}; private report retained at {}",
                cleanup.is_ok(),
                input.report.display()
            ));
        }
        Ok(fixture)
    }

    fn prepare_native_source(&mut self, bootstrap: &InstallManifest) -> Result<(), String> {
        for manifest in [bootstrap, &self.candidate] {
            let identity = required(
                self.runner.as_ref(),
                &binary(manifest, "devcoordinator2")?,
                vec!["--build-identity".into()],
            )?;
            let identity: Value = serde_json::from_str(&identity.stdout)
                .map_err(|_| "native rehearsal build identity is invalid")?;
            if identity["source_commit"] != manifest.source_commit
                || identity["protocol_version"] != 3
                || identity["root_acceptance"] != false
            {
                return Err("native rehearsal requires exact normal v3 release binaries".into());
            }
        }
        let source_commit = install::validate_live_checkout(
            Path::new(&self.candidate.source_root),
            false,
            self.runner.as_ref(),
        )?;
        if source_commit != self.candidate.source_commit {
            return Err("native rehearsal source checkout does not match its candidate".into());
        }
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "-c".into(),
                format!("safe.directory={}", self.candidate.source_root).into(),
                "-C".into(),
                self.candidate.source_root.clone().into(),
                "merge-base".into(),
                "--is-ancestor".into(),
                bootstrap.source_commit.clone().into(),
                self.candidate.source_commit.clone().into(),
            ],
        )?;
        let root = &self.input.work_root;
        let origin = root.join("origin.git");
        let source = root.join("source");
        let source_git_dir = Path::new(&self.candidate.source_root)
            .join(".git")
            .canonicalize()
            .map_err(|_| "native rehearsal source Git directory is unavailable")?;
        if !source_git_dir.is_dir() {
            return Err("native rehearsal source Git directory is not a directory".into());
        }
        let git_dir_text = path_text(&source_git_dir)?;
        if [self.candidate.source_root.as_str(), git_dir_text.as_str()]
            .iter()
            .any(|path| {
                path.bytes()
                    .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
            })
        {
            return Err("native rehearsal Git paths contain unsupported control characters".into());
        }
        let safe_git_dir = format!("safe.directory={git_dir_text}");
        // Local clone starts upload-pack with its own protected configuration.
        // Scope trust in both processes to this verified checkout only. Git
        // interprets the upload-pack value as a shell command, so quote its
        // sole dynamic argument instead of interpolating a path as syntax.
        let upload_pack = format!(
            "--upload-pack=/usr/bin/git -c '{}' upload-pack",
            safe_git_dir.replace('\'', "'\\''")
        );
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "-c".into(),
                format!("safe.directory={}", self.candidate.source_root).into(),
                "-c".into(),
                safe_git_dir.into(),
                "clone".into(),
                "--bare".into(),
                "--no-hardlinks".into(),
                upload_pack.into(),
                "--".into(),
                self.candidate.source_root.clone().into(),
                origin.clone().into_os_string(),
            ],
        )?;
        for (reference, commit) in [
            ("refs/heads/candidate", &self.candidate.source_commit),
            ("refs/heads/main", &bootstrap.source_commit),
        ] {
            required(
                self.runner.as_ref(),
                Path::new("/usr/bin/git"),
                vec![
                    "--git-dir".into(),
                    origin.clone().into_os_string(),
                    "update-ref".into(),
                    reference.into(),
                    commit.as_str().into(),
                ],
            )?;
        }
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "clone".into(),
                "--no-hardlinks".into(),
                "--branch".into(),
                "main".into(),
                "--".into(),
                origin.into_os_string(),
                source.clone().into_os_string(),
            ],
        )?;
        if install::validate_live_checkout(&source, false, self.runner.as_ref())?
            != bootstrap.source_commit
        {
            return Err("native rehearsal bootstrap checkout identity is invalid".into());
        }
        let mut final_candidate = self.candidate.clone();
        final_candidate.source_root = path_text(&source)?;
        let mut bootstrap = bootstrap.clone();
        bootstrap.source_root = final_candidate.source_root.clone();
        self.prior.source_root = final_candidate.source_root.clone();
        self.layout.candidate_manifest = root.join("bootstrap-candidate.json");
        self.layout.candidate_acceptance = root.join("bootstrap-inputs.json");
        install::write_manifest(&self.layout.candidate_manifest, &bootstrap, (0, 0))?;
        install::write_manifest(&root.join("candidate.json"), &final_candidate, (0, 0))?;
        let binding = NativeBinding {
            schema: 1,
            fixture_only: true,
            original_bootstrap_manifest_sha256: install::hash_file(
                self.input
                    .native_prior_manifest
                    .as_deref()
                    .ok_or("native rehearsal prior manifest is unavailable")?,
            )?,
            original_candidate_manifest_sha256: install::hash_file(&self.input.candidate_manifest)?,
            bootstrap_manifest_sha256: install::hash_file(&self.layout.candidate_manifest)?,
            candidate_manifest_sha256: install::hash_file(&root.join("candidate.json"))?,
            bootstrap: bootstrap.clone(),
            candidate: final_candidate.clone(),
        };
        let binding_path = root.join("native-bootstrap.json");
        write(
            &binding_path,
            &serde_json::to_vec_pretty(&binding)
                .map_err(|_| "cannot encode native bootstrap binding")?,
            0o600,
        )?;
        self.marker.native_binding_sha256 = Some(install::hash_file(&binding_path)?);
        write(
            &root.join("ownership.json"),
            &serde_json::to_vec(&self.marker)
                .map_err(|_| "cannot seal native fixture ownership")?,
            0o600,
        )?;
        self.candidate = bootstrap;
        self.final_candidate = Some(final_candidate);
        Ok(())
    }

    fn initialize(&mut self, edge: &install::LocalAccount) -> Result<(), String> {
        let config = &self.layout;
        let root = &self.input.work_root;
        for (path, mode) in [
            (root.join("etc"), 0o700),
            (root.join("etc/edge"), 0o700),
            (root.join("run"), 0o755),
            (root.join("bin"), 0o755),
            (root.join("state"), 0o751),
            (root.join("state/public"), 0o755),
            (root.join("service-logs"), 0o700),
            (root.join("cutover"), 0o700),
            (root.join("bugs"), 0o711),
        ] {
            directory(&path, mode, (0, 0))?;
        }
        directory(&config.edge_state_dir, 0o700, (edge.uid, edge.gid))?;
        write(
            &root.join("etc/edge/session.secret"),
            random_hex(32)?.as_bytes(),
            0o600,
        )?;
        install::write_manifest(&config.candidate_manifest, &self.candidate, (0, 0))?;
        install::write_manifest(&config.installed_manifest, &self.prior, (0, 0))?;
        let values = format!(
            "DEVCOORDINATOR2_SOCKET={}\nDEVCOORDINATOR2_STATE_DIR={}\nDEVCOORDINATOR2_SANDBOX_BRIDGE_DIR={}\nDEVCOORDINATOR2_BUGS_DIR={}\nDEVCOORDINATOR2_UNIT_PREFIX={}-test\nDEVCOORDINATOR2_SLICE={}-tests.slice\nDEVCOORDINATOR2_CLIENT_GROUP=\nDEVCOORDINATOR2_PORT_RANGE=39000-39099\nDEVCOORDINATOR2_BASE_DOMAIN=fixture.invalid\nDEVCOORDINATOR2_ROOT_EXECUTOR={}\nDEVCOORDINATOR2_DATABASE_CONFIG={}\n",
            config.socket_path.display(),
            root.join("state").display(),
            root.join("bridge").display(),
            root.join("bugs").display(),
            self.marker.namespace,
            self.marker.namespace,
            binary(&self.candidate, "devcoordinator2-executor")?.display(),
            config.postgres.config_path.display()
        );
        write(&config.instance_env, values.as_bytes(), 0o600)?;
        write(&root.join("service-logs/daemon.log"), b"", 0o600)?;
        write(&root.join("service-logs/edge.log"), b"", 0o600)?;
        let mut proof = serde_json::json!({"schema":1,"scope":KIND,"fixture_only":true,"result":"inputs_verified","work_root":root,"source_commit":self.candidate.source_commit,"binaries":self.candidate.binaries.iter().map(|binary|(binary.name.clone(),Value::String(binary.sha256.clone()))).collect::<serde_json::Map<_,_>>()});
        if self.final_candidate.is_some() {
            proof["phase"] = Value::String("bootstrap".into());
        }
        write(
            &config.candidate_acceptance,
            &serde_json::to_vec(&proof).map_err(|_| "cannot retain fixture-only input proof")?,
            0o600,
        )?;
        // This is synthetic authority, never a copy of installed resource identities.
        let sqlite = Connection::open(&config.database_path)
            .map_err(|_| "cannot create synthetic SQLite authority")?;
        for schema in [
            include_str!("../../../../persistence/tests/fixtures/sqlite-schema-34/schema.sql"),
            include_str!("../../../../persistence/tests/fixtures/sqlite-schema-34/tickets.sql"),
            include_str!("../../../../persistence/tests/fixtures/sqlite-schema-34/storage.sql"),
            include_str!(
                "../../../../persistence/tests/fixtures/sqlite-schema-34/sketch_history.sql"
            ),
        ] {
            sqlite
                .execute_batch(schema)
                .map_err(|_| "cannot initialize synthetic authority schema")?;
        }
        sqlite
            .execute(
                "INSERT INTO meta(key,value) VALUES('schema_version','34')",
                [],
            )
            .map_err(|_| "cannot mark synthetic schema")?;
        drop(sqlite);
        std::fs::set_permissions(
            &config.database_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .map_err(|_| "cannot protect synthetic authority")?;
        let repository = root.join("repository");
        directory(
            &repository,
            0o750,
            (self.input.caller_uid, self.input.caller_gid),
        )?;
        let as_user = |args: Vec<OsString>| {
            let mut selected = vec![
                format!("--reuid={}", self.input.caller_uid).into(),
                format!("--regid={}", self.input.caller_gid).into(),
                "--clear-groups".into(),
                "--".into(),
                "/usr/bin/git".into(),
            ];
            selected.extend(args);
            required(
                self.runner.as_ref(),
                Path::new("/usr/bin/setpriv"),
                selected,
            )
        };
        as_user(vec![
            "init".into(),
            "--quiet".into(),
            repository.as_os_str().to_owned(),
        ])?;
        let fifo = root.join("release.fifo");
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/mkfifo"),
            vec!["--mode=0666".into(), fifo.as_os_str().to_owned()],
        )?;
        let declaration = format!(
            "schema = 2\n[test]\ndefault = \"unit\"\n[test.unit]\ntimeout_seconds = 120\n[[test.unit.check]]\nname = \"main\"\ntier = \"release\"\ncommand = [\"/usr/bin/cat\", {:?}]\n",
            fifo.to_string_lossy()
        );
        install::atomic_file(
            &repository.join(".devcoordinator.toml"),
            declaration.as_bytes(),
            0o644,
            Some((self.input.caller_uid, self.input.caller_gid)),
        )?;
        as_user(vec![
            "-C".into(),
            repository.as_os_str().to_owned(),
            "add".into(),
            ".devcoordinator.toml".into(),
        ])?;
        as_user(vec![
            "-C".into(),
            repository.as_os_str().to_owned(),
            "-c".into(),
            "user.name=InstallerFixture".into(),
            "-c".into(),
            "user.email=fixture@example.invalid".into(),
            "commit".into(),
            "--quiet".into(),
            "-m".into(),
            "synthetic installer fixture".into(),
        ])?;
        self.repository_id = devcoordinator2_control::ids::repository_id(&repository)
            .map_err(|_| "synthetic repository identity is unavailable")?;
        let worktree_id = devcoordinator2_control::ids::worktree_id(&repository)
            .map_err(|_| "synthetic worktree identity is unavailable")?;
        let sqlite = Connection::open(&config.database_path)
            .map_err(|_| "synthetic source is unavailable")?;
        sqlite.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,?2,'Installer fixture','t',?3,'t')",rusqlite::params![self.repository_id,repository.to_string_lossy(),self.input.caller_uid]).map_err(|_|"cannot seed synthetic repository")?;
        sqlite
            .execute(
                "INSERT INTO worktrees VALUES(?1,?2,?3,'t','t')",
                rusqlite::params![
                    worktree_id,
                    self.repository_id,
                    repository.to_string_lossy()
                ],
            )
            .map_err(|_| "cannot seed synthetic worktree")?;
        drop(sqlite);
        let own = std::env::current_exe().map_err(|_| "rehearsal client is unavailable")?;
        let helper = root.join("bin/fixture-client");
        atomic_copy(&own, &helper, (0, 0))?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| "cannot expose fixture client to its caller")?;
        if install::hash_file(&own)? != install::hash_file(&helper)? {
            return Err("fixture client copy did not retain its exact binary identity".into());
        }
        let source = install::edge_source::prepare(
            &self.prior,
            &config.source_cache_dir,
            (0, 0),
            self.runner.as_ref(),
        )?;
        install::ensure_edge_source_access(
            Path::new(&self.candidate.source_root),
            "devcoordinator2-edge",
            self.runner.as_ref(),
        )?;
        let environment = format!(
            "EDGE_BASE_DOMAIN=fixture.invalid\nEDGE_HTTP_ONLY=1\nEDGE_HTTP_PORT=8080\nEDGE_TRUST_LOCAL_CONSOLE=1\nEDGE_ROUTES_FILE={}\nEDGE_STATE_DIR={}\nEDGE_DAEMON_SOCKET={}\nEDGE_CONSOLE_DIR={}\nEDGE_UI_SOURCE_REVISION=git:{}\n",
            root.join("state/public/routes.json").display(),
            config.edge_state_dir.display(),
            config.socket_path.display(),
            source.checkout.join("console").display(),
            source.source_commit
        );
        write(&config.edge_env, environment.as_bytes(), 0o640)?;
        let (daemon, edge_unit) = render_host_units(
            config,
            Path::new(&self.candidate.source_root),
            &binary(&self.prior, "devcoordinator2")?,
        )?;
        let daemon = daemon
            .lines()
            .filter(|line| !line.starts_with("Requires="))
            .map(|line| line.replace(&config.postgres.unit, ""))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        for (path, name) in [
            (&config.daemon_unit_path, &config.daemon_unit),
            (&config.edge_unit_path, &config.edge_unit),
            (&config.postgres.unit_path, &config.postgres.unit),
            (
                &config.tests_slice_unit_path,
                &format!("{}-tests.slice", self.marker.namespace),
            ),
        ] {
            match path.symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => {
                    return Err(
                        "fixture unit path already exists or is unavailable; foreign state was preserved"
                            .into(),
                    );
                }
            }
            let loaded = execute(
                self.runner.as_ref(),
                &config.systemctl,
                vec![
                    "show".into(),
                    "--all".into(),
                    "--property=LoadState".into(),
                    "--property=ActiveState".into(),
                    "--property=FragmentPath".into(),
                    "--property=SourcePath".into(),
                    "--property=DropInPaths".into(),
                    "--property=ControlGroup".into(),
                    "--property=Transient".into(),
                    name.into(),
                ],
            )?;
            if !fixture_unit_available(name, &loaded) {
                return Err(
                    "fixture unit name is already loaded; foreign state was preserved".into(),
                );
            }
        }
        self.runtime_prepared = true;
        write(&config.daemon_unit_path, daemon.as_bytes(), 0o644)?;
        write(&config.edge_unit_path, edge_unit.as_bytes(), 0o644)?;
        install::edge_source::bind(
            &source,
            &config.edge_unit_path,
            &config.edge_env,
            "devcoordinator2-edge",
            0,
            self.runner.as_ref(),
        )?;
        write(
            &config.tests_slice_unit_path,
            render_tests_slice_unit(Path::new(&self.candidate.source_root))?.as_bytes(),
            0o644,
        )?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["start", &config.daemon_unit])?;
        self.systemctl(&["start", &config.edge_unit])?;
        self.wait_ping(2, &self.prior.source_commit)?;
        self.edge_ping(2, &self.prior.source_commit)?;
        data(request(
            &config.socket_path,
            &envelope(
                2,
                OperationId::DecisionRecord,
                serde_json::json!({"path":repository,"aspect":"testing","title":"Before migration fixture","body":"Synthetic authority history must survive the complete installer path.","ref":"INSTALLER-REHEARSAL-BEFORE"}),
            ),
        )?)?;
        Ok(())
    }

    fn systemctl(&self, args: &[&str]) -> Result<install::CommandOutput, String> {
        required(
            self.runner.as_ref(),
            &self.layout.systemctl,
            args.iter().map(OsString::from).collect(),
        )
    }
    fn verify_service_isolation(&self) -> Result<(), String> {
        let root = service_root(&self.layout)?;
        let daemon = self.systemctl(&[
            "show",
            "--property=ProtectSystem",
            "--property=PrivateTmp",
            "--property=ReadWritePaths",
            &self.layout.daemon_unit,
        ])?;
        let writable = format!("ReadWritePaths={}", root.display());
        for expected in ["ProtectSystem=strict", "PrivateTmp=yes", &writable] {
            if !daemon.stdout.lines().any(|line| line == expected) {
                return Err("effective fixture daemon write isolation is incomplete".into());
            }
        }
        let edge =
            self.systemctl(&["show", "--property=PrivateNetwork", &self.layout.edge_unit])?;
        if !edge.stdout.lines().any(|line| line == "PrivateNetwork=yes") {
            return Err("fixture edge is not isolated from shared HTTP listeners".into());
        }
        Ok(())
    }
    fn wait_ping(&self, protocol: u8, commit: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if request(
                &self.layout.socket_path,
                &envelope(protocol, OperationId::Ping, serde_json::json!({})),
            )
            .ok()
            .is_some_and(|value| {
                value["protocol"] == protocol
                    && value["ok"] == true
                    && value["data"]["source_commit"] == commit
                    && value["data"]["schema_version"]
                        == if protocol == 2 {
                            34
                        } else {
                            DATABASE_SCHEMA_VERSION
                        }
            }) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("isolated daemon readiness did not match its explicit source".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn edge_ping(&self, protocol: u8, commit: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let result = (|| {
                let pid = self
                    .systemctl(&[
                        "show",
                        "--property=MainPID",
                        "--value",
                        &self.layout.edge_unit,
                    ])?
                    .stdout
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "isolated edge has no bound process")?;
                if pid == 0 {
                    return Err("isolated edge is not active".into());
                }
                let output = required(
                    self.runner.as_ref(),
                    Path::new("/usr/bin/nsenter"),
                    vec![
                        "--target".into(),
                        pid.to_string().into(),
                        "--net".into(),
                        "--".into(),
                        "/usr/bin/curl".into(),
                        "--silent".into(),
                        "--show-error".into(),
                        "--fail".into(),
                        "--max-time".into(),
                        "5".into(),
                        "--request".into(),
                        "POST".into(),
                        "--header".into(),
                        "Content-Type: application/json".into(),
                        "--data-binary".into(),
                        "{}".into(),
                        "--header".into(),
                        "Host: console.fixture.invalid".into(),
                        format!("http://127.0.0.1:8080/api/v{protocol}/ping").into(),
                    ],
                )
                .map_err(|_| "isolated edge POST ping failed; private command evidence retained")?;
                let value: Value = serde_json::from_str(&output.stdout)
                    .map_err(|_| "isolated edge response is invalid")?;
                if value["protocol"] != protocol
                    || value["ok"] != true
                    || value["data"]["source_commit"] != commit
                {
                    return Err("isolated edge and daemon source do not match".into());
                }
                Ok(())
            })();
            if result.is_ok() {
                return result;
            }
            if Instant::now() >= deadline {
                return result;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn exercise(&mut self, report: &mut Report) -> Result<(), String> {
        self.verify_service_isolation()?;
        report
            .checks
            .push("synthetic_prior_daemon_edge_and_authority_verified".into());
        report
            .checks
            .push("effective_marker_bound_service_write_and_network_isolation_verified".into());
        let original = self
            .layout
            .database_path
            .symlink_metadata()
            .map_err(|_| "synthetic source is unavailable")?;
        let first = self.layout.clone();
        // Ordinary activation must reject the fixture-only proof.
        if HostCutover::new_owned(first.clone(), self.runner.clone(), (0, 0)).is_ok() {
            return Err("fixture-only proof was accepted as production acceptance".into());
        }
        let mut foreign = first.clone();
        foreign.socket_path = "/run/devcoordinator2/daemon.sock".into();
        if verify_fixture_inputs(&foreign, &self.candidate, 0).is_ok() {
            return Err("fixture factory accepted an installed endpoint".into());
        }
        foreign = first.clone();
        foreign.daemon_unit = "devcoordinator2.service".into();
        foreign.daemon_unit_path = "/etc/systemd/system/devcoordinator2.service".into();
        if verify_fixture_inputs(&foreign, &self.candidate, 0).is_ok() {
            return Err("fixture factory accepted an installed unit".into());
        }
        let alias = self.input.work_root.join("foreign-alias");
        std::os::unix::fs::symlink("/var/lib/devcoordinator2", &alias)
            .map_err(|_| "cannot check fixture alias refusal")?;
        foreign = first.clone();
        foreign.database_path = alias.join("authority.sqlite3");
        let refused = verify_fixture_inputs(&foreign, &self.candidate, 0).is_err();
        std::fs::remove_file(&alias).map_err(|_| "cannot retire exact negative fixture alias")?;
        if !refused {
            return Err("fixture factory followed a foreign state alias".into());
        }
        for (link, name) in [
            (&first.cli_link, "devcoordinator2"),
            (&first.tooling_link, "devcoordinator2-tooling"),
        ] {
            let target = binary(&self.candidate, name)?;
            std::os::unix::fs::symlink(&target, link)
                .map_err(|_| "cannot check intended fixture link identity")?;
            let allowed = verify_fixture_inputs(&first, &self.candidate, 0);
            std::fs::remove_file(link)
                .map_err(|_| "cannot retire exact intended fixture test link")?;
            allowed?;
            std::os::unix::fs::symlink(self.input.work_root.join("bin/fixture-client"), link)
                .map_err(|_| "cannot check foreign fixture link refusal")?;
            let refused = verify_fixture_inputs(&first, &self.candidate, 0).is_err();
            std::fs::remove_file(link)
                .map_err(|_| "cannot retire exact foreign fixture test link")?;
            if !refused {
                return Err("fixture factory accepted an unverified managed link target".into());
            }
        }
        let env_alias = self.input.work_root.join("foreign-env");
        std::os::unix::fs::symlink(&first.edge_env, &env_alias)
            .map_err(|_| "cannot check fixture environment alias refusal")?;
        foreign = first.clone();
        foreign.edge_env = env_alias.clone();
        let refused = verify_fixture_inputs(&foreign, &self.candidate, 0).is_err();
        std::fs::remove_file(&env_alias)
            .map_err(|_| "cannot retire exact environment alias fixture")?;
        if !refused {
            return Err("fixture factory followed an environment leaf alias".into());
        }
        report.checks.push(
            "fixture_factory_refuses_installed_paths_units_aliases_and_release_admission".into(),
        );
        let mut failing =
            HostCutover::new_owned_checked(first.clone(), self.runner.clone(), (0, 0), true)?;
        failing.rehearsal_fail_edge_start = true;
        let repository = self.input.work_root.join("repository");
        let accepted = data(client_as(
            &self.input.work_root.join("bin/fixture-client"),
            &first.socket_path,
            self.input.caller_uid,
            self.input.caller_gid,
            &envelope(
                2,
                OperationId::TestStart,
                serde_json::json!({"path":repository}),
            ),
        )?)?;
        let accepted_run = accepted["run_id"]
            .as_str()
            .filter(|_| accepted["attached"] == false)
            .ok_or("fixture did not create its own accepted run")?
            .to_owned();
        let client = self.input.work_root.join("bin/fixture-client");
        let socket = first.socket_path.clone();
        let runtime = first.runtime_dir.clone();
        let uid = self.input.caller_uid;
        let gid = self.input.caller_gid;
        let repo = repository.clone();
        let owned = self.marker.clone();
        let drain = std::thread::spawn(move || -> Result<(), String> {
            let result = (|| -> Result<(), String> {
                let deadline = Instant::now() + Duration::from_secs(60);
                while !runtime.join(DRAIN_FILE).is_file() {
                    if Instant::now() >= deadline {
                        return Err("actual installer did not close fixture admission".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let attached = data(client_as(
                    &client,
                    &socket,
                    uid,
                    gid,
                    &envelope(2, OperationId::TestStart, serde_json::json!({"path":repo})),
                )?)?;
                if attached["attached"] != true || attached["run_id"] != accepted_run {
                    return Err("installer drain changed the accepted fixture run identity".into());
                }
                let refused = client_as(
                    &client,
                    &socket,
                    uid,
                    gid,
                    &envelope(
                        2,
                        OperationId::TestStart,
                        serde_json::json!({"path":repo,"mode":"replace"}),
                    ),
                )?;
                if refused["error"]["code"] != "tests_draining" {
                    return Err(
                        "actual installer admission did not reject another fixture run".into(),
                    );
                }
                Ok(())
            })();
            // Release the accepted run even when a drain assertion fails.
            // Its explicit governed timeout remains the final bound if release
            // and the normal cancellation API both fail.
            if let Err(release) =
                release_fixture_fifo(&owned, Instant::now() + Duration::from_secs(5))
            {
                let stopped = (|| -> Result<(), String> {
                    if marker(&owned.root, 0)? != owned {
                        return Err("fixture ownership changed; test.stop refused".into());
                    }
                    let stopped = data(client_as(
                        &client,
                        &socket,
                        uid,
                        gid,
                        &envelope(
                            2,
                            OperationId::TestStop,
                            serde_json::json!({"path":repo,"reason":"isolated installer fixture watchdog"}),
                        ),
                    )?)?;
                    if stopped["run_id"] != accepted_run {
                        return Err("fixture cancellation returned another run identity".into());
                    }
                    Ok(())
                })();
                return Err(format!(
                    "{}; {release}; {}",
                    result
                        .err()
                        .unwrap_or_else(|| "fixture drain observed".into()),
                    stopped
                        .err()
                        .map(|error| format!("test.stop fallback failed: {error}"))
                        .unwrap_or_else(|| "test.stop fallback verified".into())
                ));
            }
            result
        });
        let failed = activate(&mut failing);
        drain
            .join()
            .map_err(|_| "fixture drain observer failed")??;
        let error = failed
            .err()
            .ok_or("injected real edge failure did not trigger rollback")?;
        if !error.contains("without replacing the intact database") {
            return Err("real installer rollback did not preserve synthetic SQLite".into());
        }
        let mut failure_log = String::new();
        File::open(self.input.work_root.join("service-logs/edge.log"))
            .map_err(|_| "real edge failure evidence is unavailable")?
            .take(1024 * 1024)
            .read_to_string(&mut failure_log)
            .map_err(|_| "real edge failure evidence is invalid")?;
        if !failure_log.contains("EDGE_BASE_DOMAIN is required") {
            return Err(format!(
                "candidate failed outside the injected real edge startup boundary: {error}"
            ));
        }
        self.wait_ping(2, &self.prior.source_commit)?;
        self.edge_ping(2, &self.prior.source_commit)?;
        let retained = data(request(
            &self.layout.socket_path,
            &envelope(
                2,
                OperationId::TestStatus,
                serde_json::json!({"path":repository}),
            ),
        )?)?;
        if retained["run_id"] != accepted["run_id"] || retained["status"] != "passed" {
            return Err("actual installer did not drain and preserve accepted fixture work".into());
        }
        let restored = self
            .layout
            .database_path
            .symlink_metadata()
            .map_err(|_| "rollback source is unavailable")?;
        if (original.dev(), original.ino()) != (restored.dev(), restored.ino()) {
            return Err("same-schema rollback replaced the original authority identity".into());
        }
        let frozen = first.transaction_dir.join("authority-before.sqlite3");
        if !database_integrity(&frozen)?
            || !first
                .transaction_dir
                .join("postgres-backup-verified.json")
                .is_file()
        {
            return Err(
                "real rollback did not retain verified native and SQLite recovery evidence".into(),
            );
        }
        report
            .checks
            .push("real_systemd_pg_import_restart_backup_and_pre_admission_rollback".into());
        let mut second = self.layout.clone();
        second.transaction_dir = self.input.work_root.join("cutover/success");
        let mut host =
            HostCutover::new_owned_checked(second.clone(), self.runner.clone(), (0, 0), true)?;
        let receipt = activate(&mut host)?;
        if receipt.status != "activated_awaiting_final_acceptance" {
            return Err("fixture activation did not report the actual admission boundary".into());
        }
        self.wait_ping(3, &self.candidate.source_commit)?;
        self.edge_ping(3, &self.candidate.source_commit)?;
        self.verify_service_isolation()?;
        report
            .checks
            .push("real_installer_admission_and_v3_edge_api_verified".into());
        self.verify_admitted_write(
            &mut host,
            &second.transaction_dir,
            &receipt.backup,
            "INSTALLER-REHEARSAL-AFTER",
        )?;
        report.checks.push(
            "native_write_survives_service_restart_and_post_admission_rollback_is_refused".into(),
        );
        Ok(())
    }

    fn verify_admitted_write(
        &self,
        host: &mut HostCutover,
        transaction: &Path,
        backup: &str,
        reference: &str,
    ) -> Result<(), String> {
        let written = data(request(
            &self.layout.socket_path,
            &envelope(
                3,
                OperationId::DecisionRecord,
                serde_json::json!({"repository_id":self.repository_id,"aspect":"testing","title":"Post admission fixture","body":"This accepted synthetic API write must survive the PostgreSQL service restart.","ref":reference}),
            ),
        )?)?;
        let decision = written["decision_id"]
            .as_str()
            .ok_or("accepted fixture write has no authority identity")?
            .to_owned();
        self.systemctl(&["stop", &self.layout.daemon_unit])?;
        self.systemctl(&["restart", &self.layout.postgres.unit])?;
        self.systemctl(&["start", &self.layout.daemon_unit])?;
        self.wait_ping(3, &self.candidate.source_commit)?;
        self.edge_ping(3, &self.candidate.source_commit)?;
        let retained = data(request(
            &self.layout.socket_path,
            &envelope(
                3,
                OperationId::DecisionTail,
                serde_json::json!({"repository_id":self.repository_id,"n":10}),
            ),
        )?)?;
        let retained = retained.to_string();
        if !retained.contains(&decision) || !retained.contains("INSTALLER-REHEARSAL-BEFORE") {
            return Err(
                "accepted history and native API write did not survive real service restart".into(),
            );
        }
        let snapshot = read_snapshot(transaction, 0)?;
        if snapshot.schema != 3
            || snapshot.status != "committed"
            || snapshot.candidate_commit != self.candidate.source_commit
            || !host.writes_admitted()
        {
            return Err("fixture has no committed native admission to protect".into());
        }
        let reason = "isolated fixture post-admission rollback proof";
        let refused = rollback(host, &snapshot, backup, reason.into())
            .err()
            .ok_or("admitted fixture allowed stale-source rollback")?;
        if refused
            != format!(
                "PostgreSQL authority is active; recovery must preserve its new writes and roll forward: {reason}"
            )
        {
            return Err("fixture did not reach the committed PostgreSQL rollback guard".into());
        }
        self.wait_ping(3, &self.candidate.source_commit)?;
        let after_refusal = data(request(
            &self.layout.socket_path,
            &envelope(
                3,
                OperationId::DecisionTail,
                serde_json::json!({"repository_id":self.repository_id,"n":10}),
            ),
        )?)?;
        if !after_refusal.to_string().contains(&decision) {
            return Err("refused rollback changed an accepted native API write".into());
        }
        Ok(())
    }

    fn native_config(&self) -> Result<devcoordinator2_control::database::DatabaseConfig, String> {
        let config = devcoordinator2_control::database::DatabaseConfig::from_file(
            &self.layout.postgres.config_path,
        )
        .map_err(|_| "fixture PostgreSQL binding is invalid")?;
        let data = self
            .layout
            .postgres
            .data_dir
            .canonicalize()
            .map_err(|_| "fixture PostgreSQL data is unavailable")?;
        if config.data_directory.as_deref() != Some(data.as_path())
            || !contained(&data, &self.input.work_root)
        {
            return Err("fixture PostgreSQL binding selects another data directory".into());
        }
        Ok(config)
    }

    fn native_database(&self, target: Option<&str>) -> Result<(u64, u64), String> {
        let config = self.native_config()?;
        let config = match target {
            Some(name) => config.for_database(name),
            None => config,
        };
        let database = devcoordinator2_control::database::Database::connect_existing(&config)
            .map_err(|_| "fixture native authority observer could not connect")?;
        database.read(|connection| {
            Ok(connection.query_row("SELECT (SELECT oid::bigint FROM pg_database WHERE datname=current_database()),(SELECT coalesce(max(version),0) FROM dc2_schema_migrations)",[],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?)
        }).map_err(|error| {
            let _ = write(&self.artifacts.join("native-observer-error.private.txt"),format!("{error:?}").as_bytes(),0o600);
            "fixture native authority observation failed".to_owned()
        })
    }

    fn native_state(&self) -> Result<NativeState, String> {
        let (database_oid, migration) = self.native_database(None)?;
        let current_path = self
            .layout
            .postgres
            .data_dir
            .parent()
            .ok_or("fixture PostgreSQL parent is unavailable")?
            .join("authority-current.json");
        let current = finalize::private(&current_path, 0)?;
        let transaction = Path::new(
            current["transaction_dir"]
                .as_str()
                .ok_or("fixture native transaction binding is invalid")?,
        );
        if !contained(transaction, &self.input.work_root)
            || !transaction.starts_with(self.input.work_root.join("cutover"))
        {
            return Err("fixture native transaction is outside its owned root".into());
        }
        let pending = finalize::private(&transaction.join("retirement-pending.json"), 0)?;
        let targets = pending["retirement_targets"]
            .as_array()
            .filter(|targets| targets.len() <= 32)
            .ok_or("fixture native retirement targets are invalid")?;
        Ok(NativeState {
            database_oid,
            migration,
            postgres_config_sha256: install::hash_file(&self.layout.postgres.config_path)?,
            authority_marker_sha256: install::hash_file(&current_path)?,
            installed_manifest_sha256: install::hash_file(&self.layout.installed_manifest)?,
            instance_env_sha256: install::hash_file(&self.layout.instance_env)?,
            edge_env_sha256: install::hash_file(&self.layout.edge_env)?,
            edge_unit_sha256: install::hash_file(&self.layout.edge_unit_path)?,
            retirement_targets: Value::Array(targets.clone()),
        })
    }

    fn decision_history(&self) -> Result<Value, String> {
        data(request(
            &self.layout.socket_path,
            &envelope(
                3,
                OperationId::DecisionTail,
                serde_json::json!({"repository_id":self.repository_id,"n":10}),
            ),
        )?)
    }

    fn select_native_upgrade(&mut self) -> Result<(), String> {
        let binding =
            native_binding(&self.marker, 0)?.ok_or("fixture has no sealed native bootstrap")?;
        if self.final_candidate.as_ref() != Some(&binding.candidate)
            || self.candidate != binding.bootstrap
        {
            return Err("fixture native phase identities changed".into());
        }
        install::edge_source::repair_host(
            &install::edge_source::RepairConfig {
                installed_manifest: self.layout.installed_manifest.clone(),
                cache_root: self.layout.source_cache_dir.clone(),
                edge_env: self.layout.edge_env.clone(),
                edge_unit_path: self.layout.edge_unit_path.clone(),
                edge_unit: self.layout.edge_unit.clone(),
                edge_account: "devcoordinator2-edge".into(),
                runtime_dir: self.layout.runtime_dir.clone(),
                systemctl: self.layout.systemctl.clone(),
            },
            self.runner.as_ref(),
            (0, 0),
        )?;
        self.edge_ping(3, &binding.bootstrap.source_commit)?;
        let source = self.input.work_root.join("source");
        let origin = self.input.work_root.join("origin.git");
        if !contained(&source, &self.input.work_root) || !contained(&origin, &self.input.work_root)
        {
            return Err("fixture source clone identity changed".into());
        }
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "--git-dir".into(),
                origin.clone().into_os_string(),
                "update-ref".into(),
                "refs/heads/main".into(),
                binding.candidate.source_commit.clone().into(),
            ],
        )?;
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "-C".into(),
                source.clone().into_os_string(),
                "fetch".into(),
                "--no-tags".into(),
                "origin".into(),
                "refs/heads/main:refs/remotes/origin/main".into(),
            ],
        )?;
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/git"),
            vec![
                "-C".into(),
                source.clone().into_os_string(),
                "merge".into(),
                "--ff-only".into(),
                "origin/main".into(),
            ],
        )?;
        if install::validate_live_checkout(&source, false, self.runner.as_ref())?
            != binding.candidate.source_commit
        {
            return Err("fixture native candidate checkout identity is invalid".into());
        }
        self.prior = binding.bootstrap;
        self.candidate = binding.candidate;
        self.layout.candidate_manifest = self.input.work_root.join("candidate.json");
        self.layout.candidate_acceptance = self.input.work_root.join("inputs.json");
        let proof = serde_json::json!({"schema":1,"scope":KIND,"fixture_only":true,"result":"inputs_verified","phase":"upgrade","work_root":self.input.work_root,"source_commit":self.candidate.source_commit,"binaries":self.candidate.binaries.iter().map(|binary|(binary.name.clone(),Value::String(binary.sha256.clone()))).collect::<serde_json::Map<_,_>>()});
        write(
            &self.layout.candidate_acceptance,
            &serde_json::to_vec(&proof).map_err(|_| "cannot encode native upgrade proof")?,
            0o600,
        )?;
        verify_fixture_inputs(&self.layout, &self.candidate, 0)?;
        let mut foreign = self.layout.clone();
        foreign.candidate_acceptance = self.input.work_root.join("bootstrap-inputs.json");
        if verify_fixture_inputs(&foreign, &self.candidate, 0).is_ok() {
            return Err("fixture bootstrap proof authorized the native upgrade phase".into());
        }
        let mut production = self.layout.clone();
        production.transaction_dir = self.input.work_root.join("cutover/production-refusal");
        if HostCutover::new_owned(production, self.runner.clone(), (0, 0)).is_ok() {
            return Err("native fixture proof was accepted as production acceptance".into());
        }
        for link in [&self.layout.cli_link, &self.layout.tooling_link] {
            let original =
                std::fs::read_link(link).map_err(|_| "native prior managed link is unavailable")?;
            std::fs::remove_file(link)
                .map_err(|_| "cannot stage owned native link negative control")?;
            std::os::unix::fs::symlink(self.input.work_root.join("bin/fixture-client"), link)
                .map_err(|_| "cannot stage owned native link negative control")?;
            let refused = verify_fixture_inputs(&self.layout, &self.candidate, 0).is_err();
            std::fs::remove_file(link)
                .map_err(|_| "cannot retire owned native link negative control")?;
            std::os::unix::fs::symlink(original, link)
                .map_err(|_| "cannot restore exact native prior link")?;
            if !refused {
                return Err("native fixture factory accepted an unverified prior link".into());
            }
        }
        verify_fixture_inputs(&self.layout, &self.candidate, 0)?;
        self.wait_ping(3, &self.prior.source_commit)?;
        self.edge_ping(3, &self.prior.source_commit)
    }

    pub fn exercise_native(&mut self, report: &mut Report) -> Result<(), String> {
        // Preserve the complete six-check legacy journey while bootstrapping
        // the actual prior normal release, not a current-schema synthetic DB.
        self.exercise(report)?;
        if self.native_database(None)?.1 != 13 {
            return Err("verified prior release did not bootstrap native migration13".into());
        }
        self.select_native_upgrade()?;
        report
            .checks
            .push("native_fixture_phase_proofs_and_prior_link_identities_are_enforced".into());
        let accepted = data(request(
            &self.layout.socket_path,
            &envelope(
                3,
                OperationId::DecisionRecord,
                serde_json::json!({"repository_id":self.repository_id,"aspect":"testing","title":"Before native upgrade","body":"Accepted PostgreSQL13 history must survive candidate rollback and the admitted upgrade.","ref":"INSTALLER-NATIVE-BEFORE"}),
            ),
        )?)?;
        let decision_id = accepted["decision_id"]
            .as_str()
            .ok_or("prior native write has no authority identity")?
            .to_owned();
        let history = self.decision_history()?;
        let prior_record = history["decisions"]
            .as_array()
            .and_then(|records| {
                records
                    .iter()
                    .find(|record| record["decision_id"] == decision_id)
            })
            .cloned()
            .ok_or("prior native write was not readable")?;
        let before = self.native_state()?;
        report.checks.push(
            "normal_prior_release_bootstraps_real_postgres13_with_accepted_api_history".into(),
        );
        let mut failed_config = self.layout.clone();
        failed_config.transaction_dir = self.input.work_root.join("cutover/native-rollback");
        let mut failing = HostCutover::new_owned_checked(
            failed_config.clone(),
            self.runner.clone(),
            (0, 0),
            true,
        )?;
        let failed_database = failing.config.postgres.database_name.clone();
        let edge_log_path = self.input.work_root.join("service-logs/edge.log");
        let edge_before = edge_log_path
            .symlink_metadata()
            .map_err(|_| "native edge log identity is unavailable")?;
        if !edge_before.is_file() || edge_before.file_type().is_symlink() || edge_before.uid() != 0
        {
            return Err("native edge log is outside its owned evidence binding".into());
        }
        failing.rehearsal_fail_edge_start = true;
        let error = activate(&mut failing)
            .err()
            .ok_or("native candidate fault did not trigger rollback")?;
        let mut edge_log = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&edge_log_path)
            .map_err(|_| "native edge fault evidence is unavailable")?;
        let edge_after = edge_log
            .metadata()
            .map_err(|_| "native edge log identity is unavailable")?;
        if (edge_before.dev(), edge_before.ino()) != (edge_after.dev(), edge_after.ino())
            || edge_after.len() < edge_before.len()
        {
            return Err("native edge log changed identity during the fault".into());
        }
        edge_log
            .seek(SeekFrom::Start(edge_before.len()))
            .map_err(|_| "cannot inspect bounded native edge fault evidence")?;
        let mut fault = String::new();
        edge_log
            .take(1024 * 1024)
            .read_to_string(&mut fault)
            .map_err(|_| "native edge fault evidence is invalid")?;
        if !error.contains("without replacing the intact database")
            || !fault.contains("EDGE_BASE_DOMAIN is required")
        {
            return Err(format!(
                "native candidate failed outside the injected edge startup boundary: {error}"
            ));
        }
        let snapshot = read_snapshot(&failed_config.transaction_dir, 0)?;
        let verified = finalize::private(
            &failed_config
                .transaction_dir
                .join("native-source-rollback-verified.json"),
            0,
        )?;
        if snapshot.schema != 3
            || snapshot.status != "rolled_back"
            || verified["verified"] != true
            || before != self.native_state()?
            || history != self.decision_history()?
        {
            return Err("native rollback did not restore the exact prior authority binding and accepted history".into());
        }
        self.wait_ping(3, &self.prior.source_commit)?;
        self.edge_ping(3, &self.prior.source_commit)?;
        let expected_migration = devcoordinator2_control::persistence::schema::MIGRATIONS
            .last()
            .ok_or("native migration catalog is empty")?
            .0 as u64;
        if expected_migration <= 13
            || self.native_database(Some(&failed_database))?.1 != expected_migration
        {
            return Err(
                "failed native candidate did not reach the candidate migration before rollback"
                    .into(),
            );
        }
        report.checks.push(
            "real_native_pre_admission_failure_restores_postgres13_binding_data_and_edge".into(),
        );
        let mut retry = self.layout.clone();
        retry.transaction_dir = self.input.work_root.join("cutover/native-success");
        let mut host =
            HostCutover::new_owned_checked(retry.clone(), self.runner.clone(), (0, 0), true)?;
        if host.config.postgres.database_name == failed_database {
            return Err("native retry reused the failed candidate database".into());
        }
        let receipt = activate(&mut host)?;
        self.wait_ping(3, &self.candidate.source_commit)?;
        self.edge_ping(3, &self.candidate.source_commit)?;
        self.verify_service_isolation()?;
        let after = self.native_state()?;
        if receipt.status != "activated_awaiting_final_acceptance"
            || after.migration != expected_migration
            || after.database_oid == before.database_oid
            || after.retirement_targets != before.retirement_targets
        {
            return Err("native retry did not admit a fresh migrated authority with inherited retirement targets".into());
        }
        report.checks.push(
            "native_retry_admits_fresh_candidate_and_preserves_exact_retirement_targets".into(),
        );
        self.verify_admitted_write(
            &mut host,
            &retry.transaction_dir,
            &receipt.backup,
            "INSTALLER-NATIVE-AFTER",
        )?;
        let retained = self.decision_history()?;
        if retained["decisions"].as_array().and_then(|records| {
            records
                .iter()
                .find(|record| record["decision_id"] == decision_id)
        }) != Some(&prior_record)
        {
            return Err("admitted native upgrade changed accepted prior PostgreSQL history".into());
        }
        report
            .checks
            .push("native_upgrade_writes_survive_restart_and_committed_rollback_is_refused".into());
        Ok(())
    }

    pub fn cleanup(&mut self) -> Result<(), String> {
        let checked = marker(&self.input.work_root, 0)?;
        if checked != self.marker {
            return Err("fixture ownership changed; cleanup refused".into());
        }
        let mut units = vec![
            self.layout.edge_unit.clone(),
            self.layout.daemon_unit.clone(),
            self.layout.postgres.unit.clone(),
            format!("{}-tests.slice", self.marker.namespace),
        ];
        if self.runtime_prepared {
            let listed = execute(
                self.runner.as_ref(),
                &self.layout.systemctl,
                vec![
                    "list-units".into(),
                    "--all".into(),
                    "--plain".into(),
                    "--full".into(),
                    "--no-legend".into(),
                    format!("{}-test-*.service", self.marker.namespace).into(),
                ],
            )?;
            if !listed.success || listed.stdout_truncated {
                return Err("fixture process ownership listing is incomplete".into());
            }
            for line in listed.stdout.lines() {
                let unit = line
                    .split_whitespace()
                    .next()
                    .ok_or("fixture process ownership is invalid")?;
                if !unit.starts_with(&format!("{}-test-", self.marker.namespace))
                    || !unit.ends_with(".service")
                {
                    return Err("foreign runtime appeared in fixture cleanup listing".into());
                }
                units.push(unit.to_owned());
            }
            for unit in &units {
                let _ = execute(
                    self.runner.as_ref(),
                    &self.layout.systemctl,
                    vec!["stop".into(), "--no-block".into(), unit.into()],
                );
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            for unit in &units {
                loop {
                    let active = execute(
                        self.runner.as_ref(),
                        &self.layout.systemctl,
                        vec![
                            "show".into(),
                            "--property=ActiveState".into(),
                            "--property=MainPID".into(),
                            unit.into(),
                        ],
                    )?;
                    if active
                        .stdout
                        .lines()
                        .any(|line| matches!(line, "ActiveState=inactive" | "ActiveState=failed"))
                        && (unit.ends_with(".slice")
                            || active.stdout.lines().any(|line| line == "MainPID=0"))
                    {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(
                            "marker-owned fixture unit has not stopped; data retained".into()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = execute(
                    self.runner.as_ref(),
                    &self.layout.systemctl,
                    vec!["disable".into(), "--runtime".into(), unit.into()],
                );
                let link = Path::new("/run/systemd/system/multi-user.target.wants").join(unit);
                if let Ok(metadata) = link.symlink_metadata() {
                    if !metadata.file_type().is_symlink() {
                        return Err("fixture enablement link was replaced; cleanup refused".into());
                    }
                    let target = std::fs::read_link(&link)
                        .map_err(|_| "cannot inspect exact fixture enablement link")?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        link.parent().unwrap().join(target)
                    };
                    let expected = Path::new("/run/systemd/system").join(unit);
                    if target != expected && target != link.parent().unwrap().join("..").join(unit)
                    {
                        return Err("fixture enablement link belongs to another unit".into());
                    }
                    std::fs::remove_file(&link)
                        .map_err(|_| "cannot retire exact fixture enablement link")?;
                }
            }
            for path in [
                &self.layout.edge_unit_path,
                &self.layout.daemon_unit_path,
                &self.layout.postgres.unit_path,
                &self.layout.tests_slice_unit_path,
            ] {
                match path.symlink_metadata() {
                    Ok(metadata)
                        if metadata.is_file()
                            && !metadata.file_type().is_symlink()
                            && metadata.uid() == 0 =>
                    {
                        std::fs::remove_file(path)
                            .map_err(|_| "cannot retire exact fixture unit")?
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err("fixture unit identity changed; cleanup refused".into()),
                }
            }
            self.systemctl(&["daemon-reload"])?;
        }
        for commit in [&self.prior.source_commit, &self.candidate.source_commit] {
            let checkout = self.layout.source_cache_dir.join(commit);
            if checkout.exists() {
                required(
                    self.runner.as_ref(),
                    Path::new("/usr/bin/git"),
                    vec![
                        "-c".into(),
                        "safe.directory=*".into(),
                        "-C".into(),
                        self.candidate.source_root.clone().into(),
                        "worktree".into(),
                        "remove".into(),
                        "--".into(),
                        checkout.into_os_string(),
                    ],
                )?;
            }
        }
        for (source, destination) in [
            (
                self.input.work_root.join("cutover"),
                self.artifacts.join("transactions"),
            ),
            (
                self.input.work_root.join("service-logs"),
                self.artifacts.join("services"),
            ),
            (
                self.input.work_root.join("postgres/data/log"),
                self.artifacts.join("postgres-logs"),
            ),
        ] {
            if source.exists() {
                copy_cold(&source, &destination)?;
            }
        }
        atomic_copy(
            &self.input.work_root.join("ownership.json"),
            &self.artifacts.join("ownership.json"),
            (0, 0),
        )?;
        if self.marker.native_binding_sha256.is_some() {
            let _ = native_binding(&self.marker, 0)?
                .ok_or("native fixture binding disappeared before cleanup")?;
            for name in [
                "native-bootstrap.json",
                "bootstrap-candidate.json",
                "candidate.json",
                "bootstrap-inputs.json",
                "inputs.json",
            ] {
                let source = self.input.work_root.join(name);
                if source.symlink_metadata().is_ok() {
                    atomic_copy(&source, &self.artifacts.join(name), (0, 0))?;
                }
            }
        }
        marker(&self.input.work_root, 0)?;
        std::fs::remove_dir_all(&self.input.work_root)
            .map_err(|_| "cannot retire exact synthetic fixture root")?;
        Ok(())
    }
}

fn copy_cold(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = source
        .symlink_metadata()
        .map_err(|_| "fixture cold evidence is unavailable")?;
    if metadata.file_type().is_symlink() {
        return Err("fixture cold evidence traverses an alias".into());
    }
    if metadata.is_dir() {
        directory(destination, 0o700, (0, 0))?;
        for entry in
            std::fs::read_dir(source).map_err(|_| "cannot enumerate private fixture evidence")?
        {
            let entry = entry.map_err(|_| "cannot enumerate private fixture evidence")?;
            copy_cold(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        atomic_copy(source, destination, (0, 0))?;
    } else {
        return Err("fixture cold evidence has an unsupported type".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_unit_preflight_accepts_only_unbacked_inactive_slices() {
        let implicit = install::CommandOutput {
            success: true,
            stdout: "LoadState=loaded\nActiveState=inactive\nFragmentPath=\nSourcePath=\nDropInPaths=\nControlGroup=\nTransient=no\n".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let slice = "devcoordinator2-rustint-installer-fixture-tests.slice";
        assert!(fixture_unit_available(slice, &implicit));
        for name in ["fixture.service", "fixture.scope", "fixture.socket"] {
            assert!(!fixture_unit_available(name, &implicit));
        }
        let mut absent = implicit.clone();
        absent.stdout = absent
            .stdout
            .replace("LoadState=loaded", "LoadState=not-found");
        // systemctl may return nonzero for an absent unit; complete properties
        // must still prove that there is no active or configured resource.
        absent.success = false;
        assert!(fixture_unit_available("fixture.service", &absent));
        for (from, to) in [
            ("LoadState=loaded", "LoadState=masked"),
            ("ActiveState=inactive", "ActiveState=active"),
            ("ActiveState=inactive", "ActiveState=failed"),
            (
                "FragmentPath=\n",
                "FragmentPath=/run/systemd/system/fixture.slice\n",
            ),
            (
                "SourcePath=\n",
                "SourcePath=/run/systemd/generator/fixture\n",
            ),
            (
                "DropInPaths=\n",
                "DropInPaths=/run/systemd/system/fixture.slice.d/local.conf\n",
            ),
            ("ControlGroup=\n", "ControlGroup=/fixture.slice\n"),
            ("Transient=no", "Transient=yes"),
            ("Transient=no\n", ""),
            ("Transient=no", "Transient=no\nTransient=no"),
        ] {
            let mut foreign = implicit.clone();
            foreign.stdout = foreign.stdout.replace(from, to);
            assert!(!fixture_unit_available(slice, &foreign), "{from} -> {to}");
        }
        let mut failed = implicit.clone();
        failed.success = false;
        assert!(!fixture_unit_available(slice, &failed));
        let mut truncated = implicit.clone();
        truncated.stdout_truncated = true;
        assert!(!fixture_unit_available(slice, &truncated));
        truncated.stdout_truncated = false;
        truncated.stderr_truncated = true;
        assert!(!fixture_unit_available(slice, &truncated));
        absent.stdout = absent
            .stdout
            .replace("ControlGroup=\n", "ControlGroup=/foreign\n");
        assert!(!fixture_unit_available("fixture.service", &absent));
    }
}
