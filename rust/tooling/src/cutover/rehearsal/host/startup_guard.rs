//! Bounded current-v3 guard proof; never full migration/activation acceptance.
use super::*;
use devcoordinator2_control::persistence::{
    DatabaseConfig, Error as DatabaseError, Persistence, backup,
};
use std::collections::BTreeMap;
use std::sync::mpsc;

struct GuardFixture {
    input: GuardConfig,
    marker: Marker,
    layout: HostCutoverConfig,
    artifacts: PathBuf,
    runner: Arc<Recorder>,
    files: BTreeMap<PathBuf, String>,
    helper: PathBuf,
    reverse_unit: String,
}

fn generation_error(action: &str, error: DatabaseError) -> String {
    let DatabaseError::Postgres(error) = error else {
        return action.into();
    };
    let Some(error) = error.as_db_error() else {
        return action.into();
    };
    let constraint: String = error
        .constraint()
        .unwrap_or("none")
        .chars()
        .take(63)
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    format!(
        "{action} (SQLSTATE {}; constraint {constraint})",
        error.code().code()
    )
}

fn seed_generations(db: &Persistence, worktree: &Path) -> Result<(), String> {
    let worktree = path_text(worktree)?;
    db.maintenance_transaction::<_,DatabaseError,_>(|transaction| {
        // DecisionRecord has already registered this exact worktree. Reuse its
        // real binding rather than inserting its unique path a second time.
        let (repository_id, worktree_id) = transaction.query_row(
            "SELECT repository_id,worktree_id FROM worktrees WHERE worktree_path=$1",
            [&worktree],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        transaction.execute(
            "INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at) VALUES('guard-deployment',$1,$2,'guard','fixture','guard','{}','retired',7,6,'fixture',1000,'codex','fixture')",
            (&repository_id, &worktree_id),
        )?;
        transaction.execute_batch("INSERT INTO generations(deployment_id,number,path,fingerprint,created_at,state) VALUES('guard-deployment',6,'fixture-6','six','fixture','retired'),('guard-deployment',7,'fixture-7','seven','fixture','retired')")
    }).map_err(|error| generation_error("cannot seed owned deployment generations", error))?;
    check_generations(db)
}

fn check_generations(db: &Persistence) -> Result<(), String> {
    let value = db.read::<_,DatabaseError,_>(|c| {
        c.query_row("SELECT current_generation,previous_generation,(SELECT count(*) FROM generations WHERE deployment_id='guard-deployment') FROM deployments WHERE deployment_id='guard-deployment'", [],
            |r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))
    }).map_err(|error| generation_error("cannot read owned generation bindings", error))?;
    if value != (7, 6, 2) {
        return Err("owned deployment generations changed".into());
    }
    Ok(())
}

fn normal(path: &Path, commit: &str, runner: &dyn CommandRunner) -> Result<String, String> {
    if !exact(path)
        || !path.is_file()
        || path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("guard scenario requires exact normal daemon files".into());
    }
    let output = required(runner, path, vec!["--build-identity".into()])?;
    let identity: Value =
        serde_json::from_str(&output.stdout).map_err(|_| "guard daemon identity is invalid")?;
    if identity["source_commit"] != commit
        || identity["protocol_version"] != 3
        || identity["root_acceptance"] != false
    {
        return Err(
            "guard scenario cannot substitute a historical or root-acceptance daemon".into(),
        );
    }
    install::hash_file(path)
}

fn clone_source(
    source: &Path,
    destination: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let git_dir = source
        .join(".git")
        .canonicalize()
        .map_err(|_| "guard source Git directory is unavailable")?;
    if !git_dir.is_dir() {
        return Err("guard source Git directory is not a directory".into());
    }
    let source_text = path_text(source)?;
    let git_text = path_text(&git_dir)?;
    if [source_text.as_str(), git_text.as_str()]
        .iter()
        .any(|path| {
            path.bytes()
                .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
        })
    {
        return Err("guard source Git paths contain unsupported control characters".into());
    }
    let safe_git = format!("safe.directory={git_text}");
    // Local clone starts upload-pack with separate protected configuration.
    // Authorize only the verified source and its Git directory in each process;
    // Git treats upload-pack as shell syntax, so quote the dynamic argument.
    let upload_pack = format!(
        "--upload-pack=/usr/bin/git -c '{}' upload-pack",
        safe_git.replace('\'', "'\\''")
    );
    required(
        runner,
        Path::new("/usr/bin/git"),
        vec![
            "-c".into(),
            format!("safe.directory={source_text}").into(),
            "-c".into(),
            safe_git.into(),
            "clone".into(),
            "--shared".into(),
            "--no-hardlinks".into(),
            upload_pack.into(),
            "--branch".into(),
            "main".into(),
            "--".into(),
            source.to_owned().into_os_string(),
            destination.to_owned().into_os_string(),
        ],
    )?;
    Ok(())
}

fn verify_database_identity(
    config: &DatabaseConfig,
    postgres: &install::postgres::PostgresSetup,
    expected: &str,
    runner: &dyn CommandRunner,
) -> Result<Persistence, String> {
    // The runtime and privileged observation must select the same owned socket,
    // database and original runtime role. Never widen the runtime role to read a
    // superuser-only setting.
    if config.data_directory.as_deref() != Some(postgres.data_dir.as_path())
        || config.url
            != format!(
                "host={} user=devcoordinator2 dbname={expected}",
                postgres.socket_dir.display()
            )
    {
        return Err("guard fixture selected another PostgreSQL binding".into());
    }
    let db = Persistence::connect(config).map_err(|_| "owned guard PostgreSQL is unavailable")?;
    let identity = db
        .read::<_, devcoordinator2_control::persistence::Error, _>(|connection| {
            connection.query_row("SELECT current_database(),current_user", [], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
        })
        .map_err(|_| "cannot verify owned guard runtime database identity")?;
    if identity != (expected.into(), "devcoordinator2".into()) {
        return Err("guard runtime database identity does not match owned bindings".into());
    }
    // Reuse the installer's existing root-peer admin identity for this one
    // read-only observation. JSON avoids ambiguous path/column delimiters.
    let output = required(
        runner,
        &postgres.bin_dir.join("psql"),
        vec![
            "--no-psqlrc".into(),
            "--no-password".into(),
            "--host".into(),
            postgres.socket_dir.clone().into_os_string(),
            "--username=devcoordinator2_admin".into(),
            format!("--dbname={expected}").into(),
            "--set=ON_ERROR_STOP=1".into(),
            "--quiet".into(),
            "--tuples-only".into(),
            "--no-align".into(),
            "--command".into(),
            format!(
                "BEGIN READ ONLY;SET LOCAL statement_timeout={};SELECT json_build_array(current_database(),current_setting('data_directory'));ROLLBACK;",
                config.statement_timeout.as_millis().min(i32::MAX as u128)
            )
            .into(),
        ],
    )?;
    let observed: (String, String) = serde_json::from_str(output.stdout.trim())
        .map_err(|_| "owned guard admin database identity is invalid")?;
    if observed != (expected.into(), path_text(&postgres.data_dir)?) {
        return Err("guard PostgreSQL cluster identity does not match owned bindings".into());
    }
    Ok(db)
}

fn require_uninitialized_source(database: &Persistence) -> Result<(), String> {
    let empty = database
        .read::<_, devcoordinator2_control::persistence::Error, _>(|connection| {
            connection.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public')",
                [],
                |row| row.get::<_, bool>(0),
            )
        })
        .map_err(|_| "cannot inspect fresh guard source schema")?;
    if !empty {
        return Err("guard source schema was initialized before the normal base daemon".into());
    }
    Ok(())
}

fn migration_version(database: &Persistence) -> Result<i64, String> {
    let version = database
        .read::<_, devcoordinator2_control::persistence::Error, _>(|connection| {
            connection.query_row(
                "SELECT max(version) FROM dc2_schema_migrations",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
        })
        .map_err(|_| "cannot inspect owned guard migration version")?;
    version
        .filter(|version| *version > 0)
        .ok_or_else(|| "normal guard daemon did not initialize its source schema".into())
}

fn layout(input: &GuardConfig, namespace: &str) -> HostCutoverConfig {
    let root = &input.work_root;
    let unit = |name: &str| format!("{namespace}-{name}");
    let runtime_unit = |name: &str| Path::new("/run/systemd/system").join(unit(name));
    HostCutoverConfig {
        candidate_acceptance: root.join("unused-guard-only-inputs.json"),
        candidate_manifest: root.join("unused-candidate.json"),
        installed_manifest: root.join("unused-installed.json"),
        source_cache_dir: root.join("sources"),
        tmpfiles_path: root.join("etc/tmpfiles"),
        transaction_dir: root.join("cutover/guard"),
        runtime_dir: root.join("run"),
        socket_path: root.join("run/daemon.sock"),
        database_path: root.join("state/authority.sqlite3"),
        daemon_unit_path: Path::new("/etc/systemd/system").join(unit("daemon.service")),
        edge_unit_path: runtime_unit("edge.service"),
        edge_env: root.join("etc/edge.env"),
        tests_slice_unit_path: runtime_unit("tests.slice"),
        cli_link: root.join("bin/devcoordinator2"),
        tooling_link: root.join("bin/tooling"),
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
            database_name: "devcoordinator2_guard_source".into(),
            bin_dir: input.postgres_bin.clone(),
            data_dir: root.join("postgres/data"),
            socket_dir: root.join("run/postgres"),
            config_path: root.join("etc/postgres.json"),
            unit_path: runtime_unit("postgres.service"),
            unit: unit("postgres.service"),
        },
    }
}

impl GuardFixture {
    fn prepare(mut input: GuardConfig) -> Result<Self, String> {
        if rustix::process::geteuid().as_raw() != 0
            || input.caller_uid == 0
            || input.caller_gid == 0
            || input.work_root.parent() != Some(Path::new("/var/lib"))
            || !input
                .work_root
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with(PREFIX))
            || input.work_root.symlink_metadata().is_ok()
            || !exact(&input.report)
            || input.report.starts_with(&input.work_root)
            || input.report.symlink_metadata().is_ok()
        {
            return Err(
                "guard scenario requires root, non-root caller and a fresh exact installer fixture"
                    .into(),
            );
        }
        if input.base_source_commit.len() != 40
            || !input
                .base_source_commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err("guard fixture base source must be a full commit".into());
        }
        if !exact(&input.work_root)
            || !exact(&input.source_root)
            || input.source_root.canonicalize().ok().as_deref() != Some(input.source_root.as_path())
        {
            return Err("guard fixture requires exact caller-owned source and data paths".into());
        }
        let artifacts = input.report.with_extension("artifacts");
        if artifacts.symlink_metadata().is_ok() {
            return Err("guard evidence directory must be new".into());
        }
        directory(&artifacts, 0o700, (0, 0))?;
        let runner = Arc::new(Recorder {
            directory: artifacts.clone(),
            next: Mutex::new(0),
            owner: (0, 0),
        });
        let source_git = |args: &[&str]| -> Result<String, String> {
            let mut command = vec![
                format!("--reuid={}", input.caller_uid).into(),
                format!("--regid={}", input.caller_gid).into(),
                "--clear-groups".into(),
                "--".into(),
                "/usr/bin/git".into(),
                "-c".into(),
                format!("safe.directory={}", input.source_root.display()).into(),
                "-C".into(),
                input.source_root.clone().into_os_string(),
            ];
            command.extend(args.iter().map(OsString::from));
            Ok(required(runner.as_ref(), Path::new("/usr/bin/setpriv"), command)?.stdout)
        };
        if source_git(&["rev-parse", "HEAD"])?.trim() != devcoordinator2_control::SOURCE_COMMIT
            || !source_git(&["status", "--porcelain", "--untracked-files=all"])?
                .trim()
                .is_empty()
        {
            return Err("guard fixture source must be the actual clean candidate".into());
        }
        normal(
            &input.base_daemon,
            &input.base_source_commit,
            runner.as_ref(),
        )?;
        normal(
            &input.candidate_daemon,
            devcoordinator2_control::SOURCE_COMMIT,
            runner.as_ref(),
        )?;
        let pg = install::account_by_name(&input.postgres_account)?;
        if pg.uid == 0 || pg.gid == 0 {
            return Err(
                "guard fixture PostgreSQL account must already exist and be non-root".into(),
            );
        }
        directory(&input.work_root, 0o711, (0, 0))?;
        let namespace = format!("{PREFIX}guard-{}-{}", std::process::id(), random_hex(4)?);
        let marker = Marker {
            schema: 1,
            kind: KIND.into(),
            fixture_only: true,
            root: input.work_root.clone(),
            namespace: namespace.clone(),
            source_commit: devcoordinator2_control::SOURCE_COMMIT.into(),
            native_binding_sha256: None,
        };
        write(
            &input.work_root.join("ownership.json"),
            &serde_json::to_vec(&marker).map_err(|_| "cannot retain guard ownership")?,
            0o600,
        )?;
        // Recovery refusal controls must never dirty the real source checkout.
        // This independent repository owns its own pin registrations; it only
        // borrows immutable objects from the reviewed candidate repository.
        let source = input.work_root.join("source");
        clone_source(&input.source_root, &source, runner.as_ref())?;
        if install::validate_live_checkout(&source, false, runner.as_ref())?
            != devcoordinator2_control::SOURCE_COMMIT
        {
            return Err("owned guard source clone differs from the reviewed candidate".into());
        }
        input.source_root = source;
        write(
            &input.work_root.join("guard-input.json"),
            &serde_json::to_vec(&input).map_err(|_| "cannot retain fixture inputs")?,
            0o600,
        )?;
        let layout = layout(&input, &namespace);
        let reverse_unit = format!("{namespace}-reverse.service");
        let helper = input.work_root.join("bin/fixture-client");
        Ok(Self {
            input,
            marker,
            layout,
            artifacts,
            runner,
            files: BTreeMap::new(),
            helper,
            reverse_unit,
        })
    }

    fn put(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
        if let Some(previous) = self.files.get(path) {
            if install::hash_file(path)? != *previous {
                return Err("owned fixture file changed; preserve it".into());
            }
        } else if path.symlink_metadata().is_ok() {
            return Err("guard fixture would replace unowned configuration".into());
        }
        write(path, bytes, mode)?;
        self.files.insert(path.into(), install::hash_file(path)?);
        Ok(())
    }
    fn ctl(&self, args: &[&str]) -> Result<install::CommandOutput, String> {
        required(
            self.runner.as_ref(),
            &self.layout.systemctl,
            args.iter().map(OsString::from).collect(),
        )
    }
    fn config(&self) -> Result<DatabaseConfig, String> {
        devcoordinator2_control::config::Config::read_postgres_config(
            &self.layout.postgres.config_path,
        )
        .map_err(|_| "guard fixture database binding is invalid".into())
    }
    fn db_identity(&self, expected: &str) -> Result<Persistence, String> {
        verify_database_identity(
            &self.config()?,
            &self.layout.postgres,
            expected,
            self.runner.as_ref(),
        )
    }
    fn ping(&self, commit: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(v) = request(
                &self.layout.socket_path,
                &envelope(3, OperationId::Ping, serde_json::json!({})),
            ) && v["ok"] == true
                && v["data"]["source_commit"] == commit
                && v["data"]["socket"].as_str() == self.layout.socket_path.to_str()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "owned guard daemon did not expose its exact source/socket identity".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn write_decision(&self, title: &str) -> Result<(), String> {
        data(client_as(
            &self.helper,
            &self.layout.socket_path,
            self.input.caller_uid,
            self.input.caller_gid,
            &envelope(
                3,
                OperationId::DecisionRecord,
                serde_json::json!({"path":self.input.work_root.join("repository"),"aspect":"testing","title":title,"body":"Owned guard-only fixture write","ref":title}),
            ),
        )?)?;
        Ok(())
    }
    fn init(&mut self) -> Result<(), String> {
        let root = self.input.work_root.clone();
        for (name, mode) in [
            ("etc", 0o700),
            ("run", 0o755),
            ("state", 0o751),
            ("state/public", 0o755),
            ("service-logs", 0o700),
            ("bin", 0o755),
            ("bugs", 0o711),
            ("cutover", 0o700),
        ] {
            directory(&root.join(name), mode, (0, 0))?;
        }
        let repo = root.join("repository");
        directory(&repo, 0o750, (self.input.caller_uid, self.input.caller_gid))?;
        required(
            self.runner.as_ref(),
            Path::new("/usr/bin/setpriv"),
            vec![
                format!("--reuid={}", self.input.caller_uid).into(),
                format!("--regid={}", self.input.caller_gid).into(),
                "--clear-groups".into(),
                "--".into(),
                "/usr/bin/git".into(),
                "init".into(),
                "--quiet".into(),
                repo.into_os_string(),
            ],
        )?;
        atomic_copy(
            &std::env::current_exe().map_err(|_| "guard client is unavailable")?,
            &self.helper,
            (0, 0),
        )?;
        std::fs::set_permissions(&self.helper, std::fs::Permissions::from_mode(0o755))
            .map_err(|_| "cannot expose owned guard client")?;
        for (path, name) in [
            (&self.layout.daemon_unit_path, &self.layout.daemon_unit),
            (&self.layout.edge_unit_path, &self.layout.edge_unit),
            (&self.layout.postgres.unit_path, &self.layout.postgres.unit),
            (
                &Path::new("/run/systemd/system").join(&self.reverse_unit),
                &self.reverse_unit,
            ),
        ] {
            if path.symlink_metadata().is_ok() {
                return Err("guard fixture unit would overwrite foreign configuration".into());
            }
            let state = execute(
                self.runner.as_ref(),
                &self.layout.systemctl,
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
            if !fixture_unit_available(name, &state) {
                return Err("guard fixture unit name is already in use".into());
            }
        }
        self.layout
            .postgres
            .prepare(&self.input.source_root, self.runner.as_ref(), (0, 0))?;
        self.files.insert(
            self.layout.postgres.unit_path.clone(),
            install::hash_file(&self.layout.postgres.unit_path)?,
        );
        self.files.insert(
            self.layout.postgres.config_path.clone(),
            install::hash_file(&self.layout.postgres.config_path)?,
        );
        let pg_unit = std::fs::read_to_string(&self.layout.postgres.unit_path)
            .map_err(|_| "cannot inspect owned PostgreSQL unit")?;
        let pg_config = self.config()?;
        if pg_unit
            .lines()
            .any(|l| l.starts_with("RuntimeDirectory=") || l.starts_with("StateDirectory="))
            || !pg_unit.contains(&format!(" -D {}", self.layout.postgres.data_dir.display()))
            || pg_config.data_directory.as_deref() != Some(self.layout.postgres.data_dir.as_path())
            || !pg_config.url.contains(&format!(
                "host={}",
                self.layout.postgres.socket_dir.display()
            ))
        {
            return Err(
                "guard PostgreSQL setup contains an unowned runtime or data fallback".into(),
            );
        }
        self.ctl(&["daemon-reload"])?;
        self.ctl(&["start", &self.layout.postgres.unit])?;
        self.layout.postgres.bootstrap(self.runner.as_ref())?;
        // The actual base executable owns its initial schema. Candidate
        // migrations here would make an older normal base unable to start.
        require_uninitialized_source(&self.db_identity("devcoordinator2_guard_source")?)?;
        let environment = format!(
            "DEVCOORDINATOR2_SOCKET={}\nDEVCOORDINATOR2_STATE_DIR={}\nDEVCOORDINATOR2_SANDBOX_BRIDGE_DIR={}\nDEVCOORDINATOR2_BUGS_DIR={}\nDEVCOORDINATOR2_UNIT_PREFIX={}-test\nDEVCOORDINATOR2_SLICE={}-tests.slice\nDEVCOORDINATOR2_CLIENT_GROUP=\nDEVCOORDINATOR2_PORT_RANGE=39000-39099\nDEVCOORDINATOR2_BASE_DOMAIN=fixture.invalid\nDEVCOORDINATOR2_DATABASE_CONFIG={}\nDEVCOORDINATOR2_CODEX_USAGE_SOURCES_FILE=\nDEVCOORDINATOR2_TELEGRAM_TOKEN_FILE=\n",
            self.layout.socket_path.display(),
            root.join("state").display(),
            root.join("bridge").display(),
            root.join("bugs").display(),
            self.marker.namespace,
            self.marker.namespace,
            self.layout.postgres.config_path.display()
        );
        self.put(
            &self.layout.instance_env.clone(),
            environment.as_bytes(),
            0o600,
        )?;
        self.set_daemon(&self.input.base_daemon.clone())?;
        self.put(
            &self.layout.edge_unit_path.clone(),
            b"[Service]\nType=simple\nPrivateNetwork=yes\nExecStart=/usr/bin/sleep infinity\n",
            0o644,
        )?;
        let reverse = Path::new("/run/systemd/system").join(&self.reverse_unit);
        self.put(
            &reverse,
            format!(
                "[Unit]\nWants={} {}\nAfter={} {}\n[Service]\nType=oneshot\nExecStart=/bin/true\n",
                self.layout.daemon_unit,
                self.layout.edge_unit,
                self.layout.daemon_unit,
                self.layout.edge_unit
            )
            .as_bytes(),
            0o644,
        )?;
        let dropin = self
            .layout
            .daemon_unit_path
            .parent()
            .unwrap()
            .join(format!("{}.d", self.layout.daemon_unit));
        std::fs::create_dir(&dropin).map_err(|_| "cannot create owned operator drop-in fixture")?;
        self.put(&dropin.join("10-operator.conf"),b"[Unit]\nConditionPathExists=/\n[Service]\nEnvironment=GUARD_OPERATOR_VALUE=preserved\n",0o644)?;
        let edge_dropin = self
            .layout
            .edge_unit_path
            .parent()
            .unwrap()
            .join(format!("{}.d", self.layout.edge_unit));
        std::fs::create_dir(&edge_dropin)
            .map_err(|_| "cannot create owned edge operator fixture")?;
        self.put(
            &edge_dropin.join("10-operator.conf"),
            b"[Unit]\nConditionPathExists=/\n",
            0o644,
        )?;
        self.ctl(&["daemon-reload"])?;
        self.ctl(&["start", &self.layout.daemon_unit])?;
        self.ctl(&["start", &self.layout.edge_unit])?;
        self.ping(&self.input.base_source_commit)?;
        let base_version = migration_version(&self.db_identity("devcoordinator2_guard_source")?)?;
        write(
            &self.artifacts.join("source-schema-bootstrap.json"),
            &serde_json::to_vec(&serde_json::json!({
                "schema":1,"source_empty_before_base":true,
                "initialized_by_source_commit":self.input.base_source_commit,
                "migration_version":base_version
            }))
            .map_err(|_| "cannot retain base schema bootstrap evidence")?,
            0o600,
        )?;
        let isolation = self.ctl(&[
            "show",
            "--property=ProtectSystem",
            "--property=PrivateTmp",
            "--property=ReadWritePaths",
            &self.layout.daemon_unit,
        ])?;
        for expected in [
            "ProtectSystem=strict".to_owned(),
            "PrivateTmp=yes".into(),
            format!("ReadWritePaths={}", root.display()),
        ] {
            if !isolation.stdout.lines().any(|v| v == expected) {
                return Err("normal guard daemon write isolation is incomplete".into());
            }
        }
        Ok(())
    }
    fn set_daemon(&mut self, binary: &Path) -> Result<(), String> {
        let (daemon, _) = render_host_units(&self.layout, &self.input.source_root, binary)?;
        self.put(
            &self.layout.daemon_unit_path.clone(),
            daemon.as_bytes(),
            0o644,
        )?;
        let manifest = self.manifest_for(binary)?;
        self.put(
            &self.layout.installed_manifest.clone(),
            &serde_json::to_vec(&manifest).map_err(|_| "cannot retain exact fixture manifest")?,
            0o600,
        )
    }
    fn manifest_for(&self, daemon: &Path) -> Result<InstallManifest, String> {
        let commit = if daemon == self.input.base_daemon {
            &self.input.base_source_commit
        } else {
            devcoordinator2_control::SOURCE_COMMIT
        };
        normal(daemon, commit, self.runner.as_ref())?;
        let directory = daemon
            .parent()
            .ok_or("fixture normal bundle has no directory")?;
        let mut binaries = Vec::new();
        for name in [
            "devcoordinator2",
            "devcoordinator2-tooling",
            "devcoordinator2-executor",
            "devcoordinator2-migrate-authority",
        ] {
            let path = directory.join(name);
            if required(self.runner.as_ref(), &path, vec!["--source-commit".into()])?
                .stdout
                .trim()
                != commit
            {
                return Err("fixture normal bundle contains another source".into());
            }
            binaries.push(install::BinaryReceipt {
                name: name.into(),
                path: path_text(&path)?,
                sha256: install::hash_file(&path)?,
                bytes: path
                    .metadata()
                    .map_err(|_| "normal fixture binary unavailable")?
                    .len(),
                source_commit: commit.into(),
            });
        }
        install::manifest(&self.input.source_root, commit, "fixture-only", binaries)
    }
    fn capture_freezing(&self) -> Result<(), String> {
        let before = self.layout.transaction_dir.join("edge-source-before.json");
        if !before.exists() {
            let prior = install::read_and_verify_prior_manifest_owned(
                &self.layout.installed_manifest,
                self.runner.as_ref(),
                0,
            )?;
            let source = install::edge_source::prepare(
                &prior,
                &self.layout.source_cache_dir,
                (0, 0),
                self.runner.as_ref(),
            )?;
            let binding = PriorEdgeSource {
                source,
                installed_manifest: self.layout.installed_manifest.clone(),
                manifest_sha256: install::hash_file(&self.layout.installed_manifest)?,
                edge_unit_path: self.layout.edge_unit_path.clone(),
                edge_env: self.layout.edge_env.clone(),
            };
            directory(&self.layout.transaction_dir, 0o700, (0, 0))?;
            write(
                &before,
                &serde_json::to_vec_pretty(&binding)
                    .map_err(|_| "cannot retain owned prior source")?,
                0o600,
            )?;
        }
        let authority = self.input.work_root.join("postgres/authority-current.json");
        let targets = [
            authority.as_path(),
            &self.layout.postgres.unit_path,
            &self.layout.postgres.config_path,
            &self.layout.instance_env,
            &self.layout.daemon_unit_path,
            &self.layout.edge_unit_path,
            &self.layout.edge_env,
            &self.layout.tests_slice_unit_path,
            &self.layout.cli_link,
            &self.layout.tooling_link,
            &self.layout.installed_manifest,
            &self.layout.tmpfiles_path,
        ];
        startup_guard::capture_freezing(
            &self.layout,
            &self.manifest_for(&self.input.candidate_daemon)?,
            true,
            &targets,
            self.runner.as_ref(),
            (0, 0),
        )
    }
    fn freeze(&self) -> Result<(), String> {
        startup_guard::stop_services(&self.layout, self.runner.as_ref(), (0, 0))
    }
    fn blocked_probe(&self) -> Result<(), String> {
        startup_guard::require_active(
            &self.layout.transaction_dir,
            &self.layout.daemon_unit,
            self.runner.as_ref(),
            0,
        )?;
        startup_guard::require_active(
            &startup_guard::edge_transaction(&self.layout.transaction_dir),
            &self.layout.edge_unit,
            self.runner.as_ref(),
            0,
        )?;
        let before = self.start_timestamp()?;
        let edge_before = self.unit_start_timestamp(&self.layout.edge_unit)?;
        let start = self.ctl(&["restart", &self.reverse_unit]);
        // Observed PID/state is required even when the dependency command fails.
        let state = startup_guard::require_stopped(
            &self.layout.systemctl,
            self.runner.as_ref(),
            &self.layout.daemon_unit,
        );
        start?;
        state?;
        startup_guard::require_stopped(
            &self.layout.systemctl,
            self.runner.as_ref(),
            &self.layout.edge_unit,
        )?;
        // A daemon that executed and immediately failed would also have PID=0.
        // Its ExecMainStartTimestamp changes, so that is not a blocked start.
        if self.start_timestamp()? != before
            || self.unit_start_timestamp(&self.layout.edge_unit)? != edge_before
        {
            return Err("guarded dependency attempt executed the daemon".into());
        }
        startup_guard::require_active(
            &self.layout.transaction_dir,
            &self.layout.daemon_unit,
            self.runner.as_ref(),
            0,
        )
    }
    fn start_timestamp(&self) -> Result<u64, String> {
        self.unit_start_timestamp(&self.layout.daemon_unit)
    }
    fn unit_start_timestamp(&self, unit: &str) -> Result<u64, String> {
        let output = self.ctl(&[
            "show",
            "--property=LoadState",
            "--property=ExecMainStartTimestampMonotonic",
            unit,
        ])?;
        if !output.stdout.lines().any(|line| line == "LoadState=loaded") {
            return Err("guarded daemon unit is not loaded".into());
        }
        output
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("ExecMainStartTimestampMonotonic="))
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| "daemon execution timestamp unavailable".into())
    }
    fn resume(&self, commit: &str, db: &str) -> Result<(), String> {
        self.db_identity(db)?;
        startup_guard::release_services(
            &self.layout.transaction_dir,
            &self.layout.daemon_unit,
            &self.layout.edge_unit,
            &self.layout.systemctl,
            self.runner.as_ref(),
            (0, 0),
        )?;
        self.ctl(&["start", &self.layout.daemon_unit])?;
        self.ctl(&["start", &self.layout.edge_unit])?;
        self.ping(commit)?;
        self.db_identity(db)?;
        Ok(())
    }
    fn exercise(&mut self, checks: &mut Vec<String>) -> Result<(), String> {
        self.init()?;
        let base_version = migration_version(&self.db_identity("devcoordinator2_guard_source")?)?;
        checks.push("fresh_source_schema_initialized_by_exact_normal_base".into());
        checks.push("normal_inputs_owned_socket_database_and_etc_unit_verified".into());
        self.write_decision("GUARD-BEFORE")?;
        self.seed_generations()?;
        self.wrapper_rejections()?;
        for case in ["before", "after", "drift"] {
            self.early_recovery(case)?;
        }
        checks.push(
            "fixture_recovery_cli_rejects_missing_and_foreign_ownership_before_mutation".into(),
        );
        checks.push(
            "hard_exit_before_and_after_guard_recovers_unchanged_prior_without_archive_claim"
                .into(),
        );
        checks
            .push("freezing_drift_and_prepared_missing_archive_refuse_without_restoration".into());
        self.ctl(&["stop", &self.layout.edge_unit])?;
        self.ctl(&["stop", &self.layout.daemon_unit])?;
        startup_guard::require_stopped(
            &self.layout.systemctl,
            self.runner.as_ref(),
            &self.layout.daemon_unit,
        )?;
        let control = self.artifacts.join("unguarded.pgdump");
        backup::dump(&self.config()?, &control).map_err(|_| "owned unguarded dump failed")?;
        self.ctl(&["restart", &self.reverse_unit])?;
        self.ping(&self.input.base_source_commit)?;
        self.db_identity("devcoordinator2_guard_source")?;
        self.write_decision("GUARD-UNPROTECTED-RESTART")?;
        self.ctl(&["stop", &self.layout.daemon_unit])?;
        if backup::verify_archive(&self.config()?, &control).is_ok() {
            return Err(
                "unguarded reverse-Wants control did not reproduce a changed authority".into(),
            );
        }
        checks.push("unprotected_reverse_wants_reproduces_frozen_backup_refusal".into());
        self.freeze()?;
        let reset = self
            .layout
            .daemon_unit_path
            .parent()
            .unwrap()
            .join(format!("{}.d/zz-reset.conf", self.layout.daemon_unit));
        self.put(&reset, b"[Unit]\nConditionPathExists=\n", 0o644)?;
        self.ctl(&["daemon-reload"])?;
        if startup_guard::require_active(
            &self.layout.transaction_dir,
            &self.layout.daemon_unit,
            self.runner.as_ref(),
            0,
        )
        .is_ok()
        {
            return Err("effective guard check accepted a reset condition".into());
        }
        self.ctl(&["restart", &self.reverse_unit])?;
        self.ping(&self.input.base_source_commit)?;
        self.db_identity("devcoordinator2_guard_source")?;
        self.write_decision("GUARD-RESET-CONDITION")?;
        self.ctl(&["stop", &self.layout.daemon_unit])?;
        if install::hash_file(&reset)? != self.files[&reset] {
            return Err("owned reset fixture changed; preserve it".into());
        }
        std::fs::remove_file(&reset).map_err(|_| "cannot remove owned reset fixture")?;
        self.files.remove(&reset);
        self.ctl(&["daemon-reload"])?;
        self.freeze()?;
        checks.push("effective_guard_rejects_real_condition_reset_that_allows_restart".into());
        let archive = self.artifacts.join("guarded.pgdump");
        let config = self.config()?;
        let baseline = std::thread::scope(|scope| -> Result<_, String> {
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let source_config = &config;
            let source_archive = &archive;
            let worker = scope.spawn(move || -> Result<_, String> {
                backup::dump(source_config, source_archive)
                    .map_err(|_| "owned guarded dump failed")?;
                ready_tx
                    .send(())
                    .map_err(|_| "backup phase observer closed")?;
                let _ = release_rx.recv();
                backup::verify_archive(source_config, source_archive)
                    .map_err(|_| "guarded authority differs after blocked starts".into())
            });
            let entered = ready_rx
                .recv_timeout(Duration::from_secs(60))
                .map_err(|_| "owned dump never reached held comparison phase".to_owned());
            let probe = entered.and_then(|_| {
                for _ in 0..3 {
                    self.blocked_probe()?;
                }
                Ok(())
            });
            drop(release_tx);
            let compared = worker.join().map_err(|_| "owned backup thread failed")?;
            probe?;
            compared
        })?;
        checks.push("guard_blocks_dependency_starts_after_dump_through_exact_comparison".into());
        // Keep the guard while the bound database is deliberately unavailable.
        // This is an owned fixture condition, removed before restoring readiness.
        let pg_unit = std::fs::read(&self.layout.postgres.unit_path)
            .map_err(|_| "cannot retain owned PostgreSQL unit")?;
        self.ctl(&["stop", &self.layout.postgres.unit])?;
        startup_guard::require_stopped(
            &self.layout.systemctl,
            self.runner.as_ref(),
            &self.layout.postgres.unit,
        )?;
        let mut unready = pg_unit.clone();
        unready.extend(
            format!(
                "\n[Unit]\nConditionPathExists={}\n",
                self.input.work_root.join("intentionally-unready").display()
            )
            .as_bytes(),
        );
        self.put(&self.layout.postgres.unit_path.clone(), &unready, 0o644)?;
        self.ctl(&["daemon-reload"])?;
        self.blocked_probe()?;
        startup_guard::require_stopped(
            &self.layout.systemctl,
            self.runner.as_ref(),
            &self.layout.postgres.unit,
        )?;
        self.put(&self.layout.postgres.unit_path.clone(), &pg_unit, 0o644)?;
        self.ctl(&["daemon-reload"])?;
        self.ctl(&["start", &self.layout.postgres.unit])?;
        self.layout.postgres.wait_ready(self.runner.as_ref())?;
        self.db_identity("devcoordinator2_guard_source")?;
        self.blocked_probe()?;
        checks.push("guard_prevents_execstart_before_and_after_bound_database_readiness".into());
        let unit_bytes =
            std::fs::read(&self.layout.daemon_unit_path).map_err(|_| "cannot retain prior unit")?;
        let manifest_bytes = std::fs::read(&self.layout.installed_manifest)
            .map_err(|_| "cannot retain prior manifest")?;
        let config_bytes = std::fs::read(&self.layout.postgres.config_path)
            .map_err(|_| "cannot retain prior DB binding")?;
        let db = self.db_identity("devcoordinator2_guard_source")?;
        db.maintenance::<_,devcoordinator2_control::persistence::Error,_>(|c|c.execute_batch("CREATE DATABASE devcoordinator2_guard_candidate OWNER devcoordinator2 TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'" )).map_err(|_|"cannot create owned candidate DB")?;
        drop(db);
        let candidate = config.for_database("devcoordinator2_guard_candidate");
        self.held_restore(&candidate, &archive)?;
        self.blocked_probe()?;
        self.set_daemon(&self.input.candidate_daemon.clone())?;
        let binding = serde_json::json!({"schema":1,"connection":format!("host={} user=devcoordinator2 dbname=devcoordinator2_guard_candidate",self.layout.postgres.socket_dir.display()),"data_directory":self.layout.postgres.data_dir});
        self.put(
            &self.layout.postgres.config_path.clone(),
            &serde_json::to_vec(&binding).map_err(|_| "cannot encode owned candidate binding")?,
            0o600,
        )?;
        self.ctl(&["daemon-reload"])?;
        self.blocked_probe()?;
        let restored = backup::verify_archive(&self.config()?, &archive)
            .map_err(|_| "owned restored candidate is not exact")?;
        if baseline.tables != restored.tables || baseline.sequences != restored.sequences {
            return Err("guard restore changed ordinary rows or identities".into());
        }
        if migration_version(&self.db_identity("devcoordinator2_guard_candidate")?)? != base_version
        {
            return Err("candidate source was migrated before its normal daemon startup".into());
        }
        self.check_generations("devcoordinator2_guard_candidate")?;
        self.resume(
            devcoordinator2_control::SOURCE_COMMIT,
            "devcoordinator2_guard_candidate",
        )?;
        let candidate_version =
            migration_version(&self.db_identity("devcoordinator2_guard_candidate")?)?;
        let expected_version = devcoordinator2_control::persistence::schema::MIGRATIONS
            .last()
            .ok_or("candidate migration catalog is empty")?
            .0;
        // The active configuration now selects the candidate. Observe the
        // original source through its captured immutable binding instead.
        let source_version = migration_version(&verify_database_identity(
            &config,
            &self.layout.postgres,
            "devcoordinator2_guard_source",
            self.runner.as_ref(),
        )?)?;
        if candidate_version != expected_version || source_version != base_version {
            return Err("normal candidate startup did not migrate only its own database".into());
        }
        write(
            &self.artifacts.join("candidate-schema-upgrade.json"),
            &serde_json::to_vec(&serde_json::json!({
                "schema":1,"base_migration_version":base_version,
                "candidate_migration_version":candidate_version,
                "initialized_by_source_commit":devcoordinator2_control::SOURCE_COMMIT,
                "original_source_migration_unchanged":true
            }))
            .map_err(|_| "cannot retain candidate schema upgrade evidence")?,
            0o600,
        )?;
        checks.push("actual_candidate_migrates_restored_copy_after_base_initialization".into());
        checks.push("guard_spans_restore_then_resumes_exact_normal_candidate".into());
        self.write_decision("GUARD-CANDIDATE")?;
        self.freeze()?;
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("owned guard-only restoration fault");
        }));
        if unwind.is_ok() {
            return Err("guard fault was not exercised".into());
        }
        self.blocked_probe()?;
        // Reconcile through disk state rather than relying on an in-memory flag.
        startup_guard::apply(
            &self.layout.transaction_dir,
            &self.layout.daemon_unit,
            Some(&self.layout.daemon_unit_path),
            false,
            &self.layout.systemctl,
            self.runner.as_ref(),
            (0, 0),
        )?;
        self.put(&self.layout.daemon_unit_path.clone(), &unit_bytes, 0o644)?;
        self.put(
            &self.layout.installed_manifest.clone(),
            &manifest_bytes,
            0o600,
        )?;
        self.put(
            &self.layout.postgres.config_path.clone(),
            &config_bytes,
            0o600,
        )?;
        self.ctl(&["daemon-reload"])?;
        self.blocked_probe()?;
        let prior = backup::verify_archive(&self.config()?, &archive)
            .map_err(|_| "prior owned authority changed during rollback")?;
        if prior.tables != baseline.tables || prior.sequences != baseline.sequences {
            return Err("rollback did not preserve prior rows and sequences".into());
        }
        self.check_generations("devcoordinator2_guard_source")?;
        self.resume(
            &self.input.base_source_commit,
            "devcoordinator2_guard_source",
        )?;
        checks.push("guard_survives_unwind_and_prior_binding_restore_before_normal_resume".into());
        for (path, hash) in &self.files {
            if install::hash_file(path)? != *hash {
                return Err("owned fixture configuration changed unexpectedly".into());
            }
        }
        checks.push("preexisting_operator_conditions_and_content_preserved".into());
        checks.push("deployment_generations_rows_and_sequence_watermarks_preserved".into());
        Ok(())
    }
    fn wrapper_rejections(&self) -> Result<(), String> {
        let invoke = |root: &Path| {
            execute(
                self.runner.as_ref(),
                &self.helper,
                vec![
                    "startup-guard-recover".into(),
                    "--work-root".into(),
                    root.as_os_str().to_owned(),
                    "--case".into(),
                    "before".into(),
                ],
            )
        };
        if invoke(&self.input.work_root.join("missing"))?.success {
            return Err("fixture recovery accepted a missing marker".into());
        }
        let marker_path = self.input.work_root.join("ownership.json");
        let original = std::fs::read(&marker_path).map_err(|_| "cannot retain fixture marker")?;
        let mut foreign = self.marker.clone();
        foreign.kind = "foreign-fixture".into();
        write(
            &marker_path,
            &serde_json::to_vec(&foreign).map_err(|_| "cannot encode foreign marker fixture")?,
            0o600,
        )?;
        let rejected = invoke(&self.input.work_root);
        write(&marker_path, &original, 0o600)?;
        if rejected?.success {
            return Err("fixture recovery accepted a foreign marker".into());
        }
        self.ping(&self.input.base_source_commit)
    }

    fn early_recovery(&mut self, case: &str) -> Result<(), String> {
        let previous = self.layout.transaction_dir.clone();
        self.layout.transaction_dir = self.input.work_root.join(format!("cutover/early-{case}"));
        let child = |command: &str| {
            execute(
                self.runner.as_ref(),
                &self.helper,
                vec![
                    command.into(),
                    "--work-root".into(),
                    self.input.work_root.clone().into_os_string(),
                    "--case".into(),
                    case.into(),
                ],
            )
        };
        let stopped = child("startup-guard-hold")?;
        if stopped.success || stopped.stdout.trim() != "owned-startup-guard-held" {
            return Err("owned early-freeze child did not reach its hard exit".into());
        }
        let snapshot_path = self
            .layout
            .transaction_dir
            .join("installation-snapshot.json");
        let bytes = std::fs::read(&snapshot_path).map_err(|_| "early-freeze snapshot missing")?;
        let mut snapshot = read_snapshot(&self.layout.transaction_dir, 0)?;
        startup_guard::require_freezing(&snapshot)?;
        if self
            .layout
            .transaction_dir
            .join("authority-before.pgdump")
            .exists()
        {
            return Err("early-freeze fixture unexpectedly has an accepted archive".into());
        }
        if case != "before" {
            self.blocked_probe()?;
        }
        if case == "after" {
            snapshot.status = "prepared".into();
            snapshot.backup_sha256 = "0".repeat(64);
            write_snapshot(&snapshot, "prepared", (0, 0))?;
            if child("startup-guard-recover")?.success {
                return Err("prepared recovery accepted a missing archive".into());
            }
            self.blocked_probe()?;
            write(&snapshot_path, &bytes, 0o600)?;
        }
        if case == "drift" {
            for fault in ["dirty_source", "stale_source", "changed_pin"] {
                let source = &self.input.source_root;
                let git = |arguments: Vec<OsString>| -> Result<String, String> {
                    let mut args = vec![
                        "-c".into(),
                        "safe.directory=*".into(),
                        "-C".into(),
                        source.clone().into_os_string(),
                    ];
                    args.extend(arguments);
                    Ok(required(self.runner.as_ref(), Path::new("/usr/bin/git"), args)?.stdout)
                };
                let dirty = source.join("guard-recovery-dirty-control");
                let before: PriorEdgeSource = serde_json::from_value(finalize::private(
                    &self.layout.transaction_dir.join("edge-source-before.json"),
                    0,
                )?)
                .map_err(|_| "owned prior source binding is invalid")?;
                let pinned = before.source.checkout.join("console/index.html");
                let original_pin =
                    std::fs::read(&pinned).map_err(|_| "owned pinned source is unavailable")?;
                let original_pin_mode = pinned
                    .metadata()
                    .map_err(|_| "owned pinned source mode is unavailable")?
                    .mode()
                    & 0o777;
                let upstream = git(vec!["rev-parse".into(), "refs/remotes/origin/main".into()])?
                    .trim()
                    .to_owned();
                match fault {
                    "dirty_source" => {
                        write(&dirty, b"owned untracked source must be preserved\n", 0o600)?
                    }
                    "stale_source" => {
                        git(vec![
                            "update-ref".into(),
                            "refs/remotes/origin/main".into(),
                            self.input.base_source_commit.clone().into(),
                        ])?;
                    }
                    _ => write(
                        &pinned,
                        b"owned changed pin must be preserved\n",
                        original_pin_mode,
                    )?,
                }
                let rejected = child("startup-guard-recover");
                let guarded = self.blocked_probe();
                let mutation_preserved = match fault {
                    "dirty_source" => std::fs::read(&dirty)
                        .is_ok_and(|bytes| bytes == b"owned untracked source must be preserved\n"),
                    "stale_source" => {
                        git(vec!["rev-parse".into(), "refs/remotes/origin/main".into()])?.trim()
                            == self.input.base_source_commit
                    }
                    _ => std::fs::read(&pinned)
                        .is_ok_and(|bytes| bytes == b"owned changed pin must be preserved\n"),
                };
                match fault {
                    "dirty_source" => std::fs::remove_file(&dirty)
                        .map_err(|_| "cannot retire owned source refusal control")?,
                    "stale_source" => {
                        git(vec![
                            "update-ref".into(),
                            "refs/remotes/origin/main".into(),
                            upstream.into(),
                        ])?;
                    }
                    _ => write(&pinned, &original_pin, original_pin_mode)?,
                }
                if rejected?.success || !mutation_preserved {
                    return Err("early recovery accepted or changed an unverified source".into());
                }
                guarded?;
            }
            let path = self.layout.instance_env.clone();
            let original = std::fs::read(&path).map_err(|_| "cannot retain drift fixture")?;
            let mut changed = original.clone();
            changed.extend(b"# owned drift must be preserved\n");
            self.put(&path, &changed, 0o600)?;
            let rejected = execute(
                self.runner.as_ref(),
                &self.helper,
                vec![
                    "startup-guard-recover".into(),
                    "--work-root".into(),
                    self.input.work_root.clone().into_os_string(),
                    "--case".into(),
                    case.into(),
                ],
            )?;
            if rejected.success
                || std::fs::read(&path).map_err(|_| "cannot inspect retained drift")? != changed
            {
                return Err("early recovery overwrote or accepted changed configuration".into());
            }
            self.blocked_probe()?;
            self.put(&path, &original, 0o600)?;
            let added = self.layout.daemon_unit_path.parent().unwrap().join(format!(
                "{}.d/50-added-foreign.conf",
                self.layout.daemon_unit
            ));
            let added_bytes = b"[Service]\nEnvironment=GUARD_FOREIGN_VALUE=preserved\n";
            self.put(&added, added_bytes, 0o644)?;
            let rejected = execute(
                self.runner.as_ref(),
                &self.helper,
                vec![
                    "startup-guard-recover".into(),
                    "--work-root".into(),
                    self.input.work_root.clone().into_os_string(),
                    "--case".into(),
                    case.into(),
                ],
            )?;
            if rejected.success
                || std::fs::read(&added).map_err(|_| "foreign drop-in was not preserved")?
                    != added_bytes
            {
                return Err("early recovery accepted or removed a new foreign drop-in".into());
            }
            self.blocked_probe()?;
            std::fs::remove_file(&added)
                .map_err(|_| "cannot remove owned added-drop-in fixture")?;
            self.files.remove(&added);
            self.ctl(&["daemon-reload"])?;
        }
        let recovered = execute(
            self.runner.as_ref(),
            &self.helper,
            vec![
                "startup-guard-recover".into(),
                "--work-root".into(),
                self.input.work_root.clone().into_os_string(),
                "--case".into(),
                case.into(),
            ],
        )?;
        let receipt: Value = serde_json::from_str(&recovered.stdout)
            .map_err(|_| "early recovery did not retain a typed receipt")?;
        if !recovered.success
            || receipt["status"] != "recovered"
            || receipt["database_restored"] != false
        {
            return Err("early-freeze production recovery did not resume original bindings".into());
        }
        self.ping(&self.input.base_source_commit)?;
        self.db_identity("devcoordinator2_guard_source")?;
        self.check_generations("devcoordinator2_guard_source")?;
        self.layout.transaction_dir = previous;
        Ok(())
    }

    fn seed_generations(&self) -> Result<(), String> {
        let db = self.db_identity("devcoordinator2_guard_source")?;
        seed_generations(&db, &self.input.work_root.join("repository"))
    }

    fn check_generations(&self, database: &str) -> Result<(), String> {
        let db = self.db_identity(database)?;
        check_generations(&db)
    }

    fn held_restore(&self, config: &DatabaseConfig, archive: &Path) -> Result<(), String> {
        std::thread::scope(|scope| {
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let (done_tx, done_rx) = mpsc::channel();
            let (leave_tx, leave_rx) = mpsc::channel::<()>();
            let worker = scope.spawn(move || {
                let _ = go_rx.recv();
                let restored = backup::restore(config, archive)
                    .map_err(|_| "owned candidate restore failed".to_owned());
                let _ = done_tx.send(());
                let _ = leave_rx.recv();
                restored
            });
            let probes = (|| {
                self.blocked_probe()?;
                drop(go_tx);
                done_rx
                    .recv_timeout(Duration::from_secs(60))
                    .map_err(|_| "owned restoration did not finish")?;
                self.blocked_probe()
            })();
            drop(leave_tx);
            let restored = worker
                .join()
                .map_err(|_| "owned restoration worker failed")?;
            probes?;
            restored
        })
    }

    fn cleanup(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        for (unit, path) in [
            (
                &self.reverse_unit,
                Path::new("/run/systemd/system").join(&self.reverse_unit),
            ),
            (&self.layout.edge_unit, self.layout.edge_unit_path.clone()),
            (
                &self.layout.daemon_unit,
                self.layout.daemon_unit_path.clone(),
            ),
            (
                &self.layout.postgres.unit,
                self.layout.postgres.unit_path.clone(),
            ),
        ] {
            if self.files.contains_key(&path)
                && let Err(e) = self.ctl(&["stop", unit]).and_then(|_| {
                    startup_guard::require_stopped(
                        &self.layout.systemctl,
                        self.runner.as_ref(),
                        unit,
                    )
                })
            {
                errors.push(e);
            }
        }
        // Preserve source fixtures and cold artifacts if any writer could remain.
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        for name in ["daemon-start-guard.json", "daemon-start-guard.active"] {
            let path = self.layout.transaction_dir.join(name);
            if path.is_file() {
                atomic_copy(&path, &self.artifacts.join(name), (0, 0))?;
            }
            let edge = startup_guard::edge_transaction(&self.layout.transaction_dir).join(name);
            if edge.is_file() {
                atomic_copy(&edge, &self.artifacts.join(format!("edge-{name}")), (0, 0))?;
            }
        }
        for name in ["guard", "early-before", "early-after", "early-drift"] {
            if let Err(e) = startup_guard::release_services(
                &self.input.work_root.join("cutover").join(name),
                &self.layout.daemon_unit,
                &self.layout.edge_unit,
                &self.layout.systemctl,
                self.runner.as_ref(),
                (0, 0),
            ) {
                errors.push(e);
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        for (path, hash) in &self.files {
            match path.symlink_metadata() {
                Ok(m)
                    if m.is_file()
                        && !m.file_type().is_symlink()
                        && m.uid() == 0
                        && install::hash_file(path).is_ok_and(|v| v == *hash) =>
                {
                    if std::fs::remove_file(path).is_err() {
                        errors.push("cannot remove exact owned fixture configuration".into());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => errors.push("owned fixture configuration identity changed; preserved".into()),
            }
        }
        for (fragment, unit) in [
            (&self.layout.daemon_unit_path, &self.layout.daemon_unit),
            (&self.layout.edge_unit_path, &self.layout.edge_unit),
        ] {
            let parent = fragment.parent().unwrap();
            let dropin = parent.join(format!("{unit}.d"));
            if dropin.is_dir() && std::fs::read_dir(&dropin).is_ok_and(|mut r| r.next().is_none()) {
                let _ = std::fs::remove_dir(dropin);
            }
        }
        if let Err(e) = self.ctl(&["daemon-reload"]) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Internal fixture child: exit after this returns without releasing the guard.
/// The marker and all paths are reconstructed from the owned fixture namespace.
fn existing(work_root: &Path, case: &str) -> Result<GuardFixture, String> {
    let metadata = work_root
        .join("ownership.json")
        .symlink_metadata()
        .map_err(|_| "guard child has no ownership marker")?;
    if rustix::process::geteuid().as_raw() != 0
        || !exact(work_root)
        || work_root.parent() != Some(Path::new("/var/lib"))
        || !work_root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX))
        || !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return Err("guard child requires its exact root-owned fixture".into());
    }
    let marker: Marker = serde_json::from_slice(
        &std::fs::read(work_root.join("ownership.json"))
            .map_err(|_| "cannot read guard fixture marker")?,
    )
    .map_err(|_| "guard fixture marker is invalid")?;
    if marker.root != work_root
        || marker.kind != KIND
        || !marker.fixture_only
        || marker.schema != 1
        || marker.source_commit != devcoordinator2_control::SOURCE_COMMIT
        || !marker.namespace.starts_with(&format!("{PREFIX}guard-"))
        || !marker
            .namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("guard child ownership does not match this source".into());
    }
    if !["before", "after", "drift"].contains(&case) {
        return Err("unknown guard recovery fixture phase".into());
    }
    let input: GuardConfig =
        serde_json::from_value(finalize::private(&work_root.join("guard-input.json"), 0)?)
            .map_err(|_| "guard child input is invalid")?;
    if input.work_root != work_root
        || input.caller_uid == 0
        || input.caller_gid == 0
        || input.report.starts_with(work_root)
    {
        return Err("guard child input belongs to another fixture".into());
    }
    let mut layout = layout(&input, &marker.namespace);
    layout.transaction_dir = work_root.join(format!("cutover/early-{case}"));
    let artifacts = input
        .report
        .with_extension("artifacts")
        .join(format!("child-{case}-{}", random_hex(4)?));
    directory(&artifacts, 0o700, (0, 0))?;
    let runner = Arc::new(Recorder {
        directory: artifacts.clone(),
        next: Mutex::new(0),
        owner: (0, 0),
    });
    normal(
        &input.candidate_daemon,
        devcoordinator2_control::SOURCE_COMMIT,
        runner.as_ref(),
    )?;
    Ok(GuardFixture {
        helper: work_root.join("bin/fixture-client"),
        reverse_unit: format!("{}-reverse.service", marker.namespace),
        input,
        marker,
        layout,
        runner,
        artifacts,
        files: BTreeMap::new(),
    })
}

pub(super) fn hold(work_root: &Path, case: &str) -> Result<(), String> {
    let fixture = existing(work_root, case)?;
    fixture.capture_freezing()?;
    if case != "before" {
        fixture.freeze()?;
    }
    Ok(())
}

pub(super) fn recover(work_root: &Path, case: &str) -> Result<Value, String> {
    let fixture = existing(work_root, case)?;
    let config = RecoveryConfig {
        transaction_dir: fixture.layout.transaction_dir.clone(),
        runtime_dir: fixture.layout.runtime_dir.clone(),
        socket_path: fixture.layout.socket_path.clone(),
        database_path: fixture.layout.database_path.clone(),
        systemctl: fixture.layout.systemctl.clone(),
        daemon_unit: fixture.layout.daemon_unit.clone(),
        edge_unit: fixture.layout.edge_unit.clone(),
        edge_state_dir: fixture.layout.edge_state_dir.clone(),
        source_root: fixture.input.source_root.clone(),
        edge_env: fixture.layout.edge_env.clone(),
    };
    recover_host(&config, fixture.runner.as_ref(), (0, 0)).and_then(|r| {
        serde_json::to_value(r).map_err(|_| "cannot encode guard recovery receipt".into())
    })
}

pub(super) fn run(input: GuardConfig) -> Result<Value, String> {
    let mut fixture = GuardFixture::prepare(input)?;
    let mut checks = Vec::new();
    let result = fixture.exercise(&mut checks);
    let cleanup = fixture.cleanup();
    let passed = result.is_ok() && cleanup.is_ok();
    let report = serde_json::json!({"schema":1,"scope":"coordinator-startup-guard-only","fixture_only":true,"full_installer_acceptance":false,
        "source_commit":devcoordinator2_control::SOURCE_COMMIT,"base_source_commit":fixture.input.base_source_commit,"namespace":fixture.marker.namespace,
        "status":if passed{"passed"}else{"failed"},"checks":checks,"cleanup_complete":cleanup.is_ok(),"error":result.err().or_else(||cleanup.err())});
    write(
        &fixture.input.report,
        &serde_json::to_vec_pretty(&report).map_err(|_| "cannot encode guard report")?,
        0o600,
    )?;
    if passed {
        Ok(report)
    } else {
        Err("guard-only scenario failed; private report and owned evidence retained".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ForeignOwner<'a>(&'a dyn CommandRunner);
    impl CommandRunner for ForeignOwner<'_> {
        fn run(&self, request: &CommandRequest) -> Result<install::CommandOutput, String> {
            let mut request = request.clone();
            request
                .environment
                .insert("GIT_TEST_ASSUME_DIFFERENT_OWNER".into(), "1".into());
            self.0.run(&request)
        }
    }

    fn git(runner: &dyn CommandRunner, args: Vec<OsString>) -> install::CommandOutput {
        required(runner, Path::new("/usr/bin/git"), args).unwrap()
    }

    fn check_clone(source: &Path, root: &Path, runner: &Recorder, foreign_owner: bool) {
        let forced = ForeignOwner(runner);
        let clone_runner: &dyn CommandRunner = if foreign_owner { &forced } else { runner };
        let failed = root.join("outer-only-source");
        assert!(
            required(
                clone_runner,
                Path::new("/usr/bin/git"),
                vec![
                    "-c".into(),
                    "safe.directory=*".into(),
                    "clone".into(),
                    "--shared".into(),
                    "--no-hardlinks".into(),
                    "--branch".into(),
                    "main".into(),
                    "--".into(),
                    source.to_owned().into_os_string(),
                    failed.into_os_string(),
                ],
            )
            .is_err()
        );
        let stem = format!("command-{:05}", *runner.next.lock().unwrap());
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(runner.directory.join(format!("{stem}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["success"], false);
        assert_eq!(metadata["stderr_truncated"], false);
        assert!(
            std::fs::read_to_string(runner.directory.join(format!("{stem}.stderr")))
                .unwrap()
                .contains("detected dubious ownership")
        );
        let destination = root.join("scoped-source");
        clone_source(source, &destination, clone_runner).unwrap();
        let expected = git(
            &install::HostRunner,
            vec![
                "-c".into(),
                format!("safe.directory={}", source.display()).into(),
                "-C".into(),
                source.to_owned().into_os_string(),
                "rev-parse".into(),
                "HEAD".into(),
            ],
        );
        assert_eq!(
            install::validate_live_checkout(&destination, false, runner).unwrap(),
            expected.stdout.trim()
        );
    }

    #[test]
    fn database_identity_preserves_runtime_privileges_and_base_bootstrap() {
        use devcoordinator2_control::persistence::{Error, fixture::FixtureCluster};
        let cluster = FixtureCluster::start().unwrap();
        let expected = "guard_identity";
        let mut config = cluster.create_database(expected).unwrap();
        let mut admin = cluster.client(expected).unwrap();
        admin.batch_execute("CREATE ROLE devcoordinator2_admin LOGIN SUPERUSER;CREATE ROLE devcoordinator2 LOGIN CREATEDB NOSUPERUSER NOCREATEROLE NOREPLICATION;ALTER DATABASE guard_identity OWNER TO devcoordinator2;").unwrap();
        let postgres = install::postgres::PostgresSetup {
            database_name: expected.into(),
            data_dir: config.data_directory.clone().unwrap(),
            socket_dir: cluster.socket_directory().into(),
            bin_dir: std::env::var_os("DEVCOORDINATOR2_TEST_POSTGRES_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| install::postgres::PostgresSetup::default().bin_dir),
            ..Default::default()
        };
        config.url = format!(
            "host={} user=devcoordinator2 dbname={expected}",
            postgres.socket_dir.display()
        );
        let runtime = Persistence::connect(&config).unwrap();
        require_uninitialized_source(&runtime).unwrap();
        let setting_denied = |runtime: &Persistence| {
            let error = runtime
                .read::<String, Error, _>(|connection| {
                    connection.query_row("SELECT current_setting('data_directory')", [], |row| {
                        row.get(0)
                    })
                })
                .unwrap_err();
            assert!(
                matches!(error, Error::Postgres(error) if error.code().is_some_and(|code| code.code() == "42501"))
            );
        };
        setting_denied(&runtime);
        let temporary = tempfile::tempdir().unwrap();
        let owner = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        let artifacts = temporary.path().join("artifacts");
        directory(&artifacts, 0o700, owner).unwrap();
        let runner = Recorder {
            directory: artifacts,
            next: Mutex::new(0),
            owner,
        };
        let verified = verify_database_identity(&config, &postgres, expected, &runner).unwrap();
        require_uninitialized_source(&verified).unwrap();
        setting_denied(&verified);
        let attributes = admin
            .query_one(
                "SELECT rolsuper,rolcreatedb,rolcreaterole,rolreplication FROM pg_roles WHERE rolname='devcoordinator2'",
                &[],
            )
            .unwrap();
        assert!(!attributes.get::<_, bool>(0));
        assert!(attributes.get::<_, bool>(1));
        assert!(!attributes.get::<_, bool>(2));
        assert!(!attributes.get::<_, bool>(3));

        let commands = *runner.next.lock().unwrap();
        let mut foreign = config.clone();
        foreign.url.push_str(" application_name=foreign");
        assert!(verify_database_identity(&foreign, &postgres, expected, &runner).is_err());
        assert_eq!(*runner.next.lock().unwrap(), commands);
        assert!(verify_database_identity(&config, &postgres, "wrong_database", &runner).is_err());
        assert_eq!(*runner.next.lock().unwrap(), commands);
        let wrong_data = temporary.path().join("foreign-data");
        directory(&wrong_data, 0o700, owner).unwrap();
        let mut false_config = config.clone();
        false_config.data_directory = Some(wrong_data.clone());
        let mut false_cluster = postgres.clone();
        false_cluster.data_dir = wrong_data;
        assert!(
            verify_database_identity(&false_config, &false_cluster, expected, &runner).is_err()
        );
        setting_denied(&runtime);

        // The root fixture proves startup through the actual base daemon. A
        // source-executor companion supplies its source-stamped offline sibling
        // here to exercise that same migration entrypoint on an owned empty DB.
        let mut base = None;
        if let Some(binary) = std::env::var_os("DEVCOORDINATOR2_TEST_BASE_MIGRATOR") {
            let binary = PathBuf::from(binary);
            let expected_commit = "ac7f337edb74250438e6926700c4578218f350ec";
            assert_eq!(
                required(&runner, &binary, vec!["--source-commit".into()])
                    .unwrap()
                    .stdout
                    .trim(),
                expected_commit
            );
            let private = temporary.path().join("postgres.json");
            install::atomic_file(
                &private,
                &serde_json::to_vec(&serde_json::json!({
                    "schema":1,"connection":config.url,"data_directory":config.data_directory
                }))
                .unwrap(),
                0o600,
                Some(owner),
            )
            .unwrap();
            let report = temporary.path().join("normal-base-migration.json");
            required(
                &runner,
                &binary,
                vec![
                    "--mode=migrate".into(),
                    "--database-config".into(),
                    private.clone().into_os_string(),
                    "--report".into(),
                    report.into_os_string(),
                ],
            )
            .unwrap();
            assert_eq!(migration_version(&runtime).unwrap(), 14);
            assert!(require_uninitialized_source(&runtime).is_err());
            base = Some((binary, private));
        }
        if base.is_none() {
            runtime.migrate().unwrap();
        }
        let repository = temporary.path().join("repository");
        git(
            &runner,
            vec![
                "init".into(),
                "--quiet".into(),
                repository.clone().into_os_string(),
            ],
        );
        let registry = devcoordinator2_control::repository::Registry::new(
            devcoordinator2_control::database::Database::connect_existing(&config).unwrap(),
        );
        let registered = registry.register(&repository, owner.0, owner.1).unwrap();
        assert!(registered.registered);
        let base_version = migration_version(&runtime).unwrap();
        assert!(seed_generations(&runtime, &temporary.path().join("unregistered")).is_err());
        seed_generations(&runtime, &repository).unwrap();
        assert_eq!(
            seed_generations(&runtime, &repository).unwrap_err(),
            "cannot seed owned deployment generations (SQLSTATE 23505; constraint deployments_pkey)"
        );
        check_generations(&runtime).unwrap();
        let binding = runtime
            .read::<_, Error, _>(|connection| {
                connection.query_row(
                    "SELECT repository_id,worktree_id,(SELECT count(*) FROM worktrees) FROM deployments WHERE deployment_id='guard-deployment'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
                )
            })
            .unwrap();
        assert_eq!(
            binding,
            (registered.repository_id, registered.worktree_id, 1)
        );

        let archive = temporary.path().join("guard-generations.pgdump");
        backup::dump(&config, &archive).unwrap();
        let baseline = backup::verify_archive(&config, &archive).unwrap();
        runtime.maintenance::<_, Error, _>(|connection| {
            connection.execute_batch("CREATE DATABASE guard_candidate OWNER devcoordinator2 TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C'")
        }).unwrap();
        let restore_config = config.for_database("guard_candidate");
        backup::restore(&restore_config, &archive).unwrap();
        let restored = backup::verify_archive(&restore_config, &archive).unwrap();
        assert_eq!(baseline.tables, restored.tables);
        assert_eq!(baseline.sequences, restored.sequences);

        let mut candidate_config = restore_config;
        candidate_config.url = format!(
            "host={} user=devcoordinator2 dbname=guard_candidate",
            postgres.socket_dir.display()
        );
        assert!(verify_database_identity(&candidate_config, &postgres, expected, &runner).is_err());
        let candidate =
            verify_database_identity(&candidate_config, &postgres, "guard_candidate", &runner)
                .unwrap();
        assert_eq!(migration_version(&candidate).unwrap(), base_version);
        check_generations(&candidate).unwrap();
        candidate.migrate().unwrap();
        assert_eq!(
            migration_version(&candidate).unwrap(),
            devcoordinator2_control::persistence::schema::MIGRATIONS
                .last()
                .unwrap()
                .0
        );
        check_generations(&candidate).unwrap();
        let source = verify_database_identity(&config, &postgres, expected, &runner).unwrap();
        assert_eq!(migration_version(&source).unwrap(), base_version);
        check_generations(&source).unwrap();
        backup::verify_archive(&config, &archive).unwrap();
        assert!(require_uninitialized_source(&runtime).is_err());
        if let Some((binary, _source_private)) = base {
            let private = temporary.path().join("candidate-postgres.json");
            install::atomic_file(
                &private,
                &serde_json::to_vec(&serde_json::json!({
                    "schema":1,"connection":candidate_config.url,"data_directory":candidate_config.data_directory
                }))
                .unwrap(),
                0o600,
                Some(owner),
            )
            .unwrap();
            let output = execute(
                &runner,
                &binary,
                vec![
                    "--mode=migrate".into(),
                    "--database-config".into(),
                    private.into_os_string(),
                    "--report".into(),
                    temporary
                        .path()
                        .join("too-new-refused.json")
                        .into_os_string(),
                ],
            )
            .unwrap();
            assert!(!output.success);
            assert!(
                output
                    .stderr
                    .contains("target schema is newer than this executable")
            );
        }
    }

    #[test]
    fn source_clone_scopes_upload_pack_and_retains_setup_failures() {
        // The real guard scenario requires root and systemd. Exercise its exact
        // Git/recording boundary here without creating services or a database.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let artifacts = root.join("artifacts");
        let owner = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        directory(&artifacts, 0o700, owner).unwrap();
        let runner = Recorder {
            directory: artifacts.clone(),
            next: Mutex::new(0),
            owner,
        };
        assert!(
            required(
                &runner,
                Path::new("/usr/bin/git"),
                vec![
                    "-C".into(),
                    root.join("missing-source").into_os_string(),
                    "rev-parse".into(),
                    "HEAD".into(),
                ],
            )
            .is_err()
        );
        assert!(
            !std::fs::read(artifacts.join("command-00001.stderr"))
                .unwrap()
                .is_empty()
        );
        let metadata = artifacts.join("command-00001.json").metadata().unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!((metadata.uid(), metadata.gid()), owner);

        // Spaces and a quote also exercise the upload-pack argument escaping.
        let source = root.join("source ' quoted");
        git(
            &install::HostRunner,
            vec![
                "init".into(),
                "--initial-branch=main".into(),
                source.clone().into_os_string(),
            ],
        );
        git(
            &install::HostRunner,
            vec![
                "-C".into(),
                source.clone().into_os_string(),
                "-c".into(),
                "user.name=fixture".into(),
                "-c".into(),
                "user.email=fixture@example.invalid".into(),
                "commit".into(),
                "--no-gpg-sign".into(),
                "--allow-empty".into(),
                "-m".into(),
                "fixture".into(),
            ],
        );
        check_clone(&source, root, &runner, true);

        // A source-executor companion may supply the actual shared checkout.
        // It is read-only; the clone owns all new files and registrations.
        if let Some(source) = std::env::var_os("DEVCOORDINATOR2_TEST_FOREIGN_SOURCE_ROOT") {
            let source = PathBuf::from(source);
            assert_ne!(source.metadata().unwrap().uid(), owner.0);
            let companion = root.join("real-owner");
            directory(&companion, 0o700, owner).unwrap();
            check_clone(&source, &companion, &runner, false);
        }
    }
}
