//! A native upgrade never overwrites its previous database; recovery restores
//! only its captured service/configuration binding while the endpoint is fenced.
use super::*;

/// Finish a durable candidate commit whose exact admission fence still exists.
/// This path never restores installation entries or an earlier database.
pub(super) fn finish_activation(
    config: &RecoveryConfig,
    snapshot: &InstallationSnapshot,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<RecoveryReceipt, String> {
    let pending_path = config.transaction_dir.join("retirement-pending.json");
    let pending = finalize::private(&pending_path, owner.0)?;
    let bound_path = |field: &str| -> Result<PathBuf, String> {
        let path = PathBuf::from(
            pending[field]
                .as_str()
                .ok_or("committed activation binding is incomplete")?,
        );
        if !path.is_absolute() {
            return Err("committed activation binding is not absolute".into());
        }
        Ok(path)
    };
    if pending["schema"] != 1
        || pending["source_commit"] != snapshot.candidate_commit
        || bound_path("socket_path")? != config.socket_path
        || pending["daemon_unit"].as_str() != Some(config.daemon_unit.as_str())
        || pending["edge_unit"].as_str() != Some(config.edge_unit.as_str())
        || owned_fence_binding(&config.runtime_dir.join("daemon.pre-cutover.sock"), owner.0)?
            != pending["fence"]
    {
        return Err("committed activation identity changed; preserve PostgreSQL authority".into());
    }
    let manifest_path = bound_path("installed_manifest")?;
    if !snapshot
        .entries
        .iter()
        .any(|entry| Path::new(&entry.path) == manifest_path)
    {
        return Err("committed activation manifest is outside its captured installation".into());
    }
    let manifest = read_and_verify_manifest_owned(&manifest_path, runner, owner.0)?;
    if manifest.source_commit != snapshot.candidate_commit
        || Path::new(&manifest.source_root) != config.source_root
        || install::validate_live_checkout(&config.source_root, false, runner)?
            != manifest.source_commit
    {
        return Err("committed activation source no longer matches the exact candidate".into());
    }
    let accepted = bound_path("candidate_acceptance")?;
    if accepted != config.transaction_dir.join("candidate-acceptance.json")
        || pending["candidate_acceptance_sha256"].as_str()
            != Some(install::postgres::verify_acceptance(&accepted, &manifest, owner.0)?.as_str())
    {
        return Err("committed activation acceptance is unverified".into());
    }
    let pg_config = bound_path("postgres_config")?;
    let private_config = finalize::private(&pg_config, owner.0)?;
    let data = PathBuf::from(
        private_config["data_directory"]
            .as_str()
            .ok_or("committed authority data binding is unavailable")?,
    )
    .canonicalize()
    .map_err(|_| "committed authority data binding is unavailable")?;
    let marker = bound_path("authority_marker")?;
    if marker
        .canonicalize()
        .map_err(|_| "committed authority marker is unavailable")?
        != data
            .parent()
            .ok_or("committed authority has no owned data parent")?
            .join("authority-current.json")
    {
        return Err("committed authority marker belongs to another cluster".into());
    }
    let current = finalize::private(&marker, owner.0)?;
    if current["schema"] != 1
        || current["source_commit"] != snapshot.candidate_commit
        || current["transaction_dir"].as_str() != config.transaction_dir.to_str()
        || current["database_name"] != pending["database_name"]
    {
        return Err("current PostgreSQL authority is not this committed candidate".into());
    }
    let instance = bound_path("instance_env")?;
    if bound_path("edge_env")? != config.edge_env {
        return Err("committed edge configuration belongs to another installation".into());
    }
    let check_bindings = || -> Result<(), String> {
        for (path, field) in [
            (&pg_config, "postgres_config_sha256"),
            (&instance, "instance_env_sha256"),
            (&config.edge_env, "edge_env_sha256"),
        ] {
            if pending[field].as_str() != Some(install::hash_file(path)?.as_str()) {
                return Err("committed service or authority configuration changed".into());
            }
        }
        let bindings = pending["service_bindings"]
            .as_array()
            .filter(|rows| rows.len() == 3)
            .ok_or("committed service bindings are incomplete")?;
        for binding in bindings {
            let path = Path::new(
                binding["path"]
                    .as_str()
                    .ok_or("committed service path is unavailable")?,
            );
            let metadata = path
                .symlink_metadata()
                .map_err(|_| "committed service unit is unavailable")?;
            if !path.is_absolute()
                || !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != owner.0
                || metadata.mode() & 0o022 != 0
                || binding["sha256"].as_str() != Some(install::hash_file(path)?.as_str())
            {
                return Err("committed service unit changed".into());
            }
        }
        if finalize::private(&marker, owner.0)? != current {
            return Err("current authority changed during committed recovery".into());
        }
        Ok(())
    };
    check_bindings()?;
    if install::hash_file(&config.transaction_dir.join("authority-before.pgdump"))?
        != snapshot.backup_sha256
    {
        return Err("committed PostgreSQL recovery archive changed".into());
    }
    let pg_unit = pending["postgres_unit"]
        .as_str()
        .filter(|name| name.ends_with(".service") && !name.starts_with('-'))
        .ok_or("committed PostgreSQL service binding is unavailable")?;
    let drain = begin_drain(&config.runtime_dir, owner)?;
    run_systemctl(runner, &config.systemctl, &["start", pg_unit])?;
    let helper = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "devcoordinator2-migrate-authority")
        .ok_or("committed migration helper is unavailable")?;
    let ready_report = config
        .transaction_dir
        .join(format!("commit-recovery-authority-{}.json", random_hex(8)?));
    let ready = runner.run(&CommandRequest {
        program: helper.path.clone().into(),
        args: vec![
            "--mode".into(),
            "inspect-postgres".into(),
            "--database-config".into(),
            pg_config.as_os_str().to_owned(),
            "--report".into(),
            ready_report.as_os_str().to_owned(),
        ],
        environment: std::collections::BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/bin:/bin"),
        )]),
        clear_environment: true,
    })?;
    if !ready.success
        || ready.stdout_truncated
        || ready.stderr_truncated
        || finalize::private(&ready_report, owner.0)?["verified"] != true
    {
        return Err("committed PostgreSQL authority is not ready; start guard remains".into());
    }
    check_bindings()?;
    startup_guard::release_services(
        &config.transaction_dir,
        &config.daemon_unit,
        &config.edge_unit,
        &config.systemctl,
        runner,
        owner,
    )?;
    run_systemctl_all(
        runner,
        &config.systemctl,
        &[("start", &config.daemon_unit), ("start", &config.edge_unit)],
    )?;
    let control = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "devcoordinator2")
        .ok_or("committed control binary is unavailable")?;
    let environment = std::collections::BTreeMap::from([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (
            OsString::from("DEVCOORDINATOR2_INSTANCE_ENV"),
            instance.as_os_str().to_owned(),
        ),
    ]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let output = runner.run(&CommandRequest {
            program: control.path.clone().into(),
            args: vec!["call".into(), "ping".into()],
            environment: environment.clone(),
            clear_environment: true,
        })?;
        let response = serde_json::from_str::<Value>(&output.stdout).ok();
        if output.success
            && !output.stdout_truncated
            && !output.stderr_truncated
            && response.as_ref().is_some_and(|value| {
                successful_v3_ping(value)
                    && value["data"]["source_commit"] == manifest.source_commit
                    && value["data"]["socket"].as_str() == config.socket_path.to_str()
            })
        {
            break;
        }
        if !response.as_ref().is_some_and(retryable_v3_ping)
            || std::time::Instant::now() >= deadline
        {
            return Err("committed candidate readiness is unverified; its fence remains".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let helper = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "devcoordinator2-migrate-authority")
        .ok_or("committed migration helper is unavailable")?;
    let report = config
        .transaction_dir
        .join(format!("commit-recovery-routes-{}.json", random_hex(8)?));
    let output = runner.run(&CommandRequest {
        program: helper.path.clone().into(),
        args: vec![
            "--mode".into(),
            "verify-routes".into(),
            "--database-config".into(),
            pg_config.as_os_str().to_owned(),
            "--state-dir".into(),
            config
                .database_path
                .parent()
                .ok_or("committed state directory is unavailable")?
                .as_os_str()
                .to_owned(),
            "--edge-state".into(),
            config.edge_state_dir.as_os_str().to_owned(),
            "--report".into(),
            report.as_os_str().to_owned(),
        ],
        environment: std::collections::BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/bin:/bin"),
        )]),
        clear_environment: true,
    })?;
    if !output.success
        || output.stdout_truncated
        || output.stderr_truncated
        || finalize::private(&report, owner.0)?["verified"] != true
    {
        return Err("committed candidate route ownership is unverified; its fence remains".into());
    }
    for (operation, params) in [
        ("repository.list", serde_json::json!({"limit":1})),
        ("deployment.list", serde_json::json!({})),
        ("health.summary", serde_json::json!({})),
    ] {
        let output = runner.run(&CommandRequest {
            program: control.path.clone().into(),
            args: vec![
                "call".into(),
                operation.into(),
                "--params".into(),
                params.to_string().into(),
            ],
            environment: environment.clone(),
            clear_environment: true,
        })?;
        if !output.success
            || output.stdout_truncated
            || output.stderr_truncated
            || !serde_json::from_str::<Value>(&output.stdout)
                .ok()
                .as_ref()
                .is_some_and(successful_v3_ping)
        {
            return Err("committed candidate read verification failed; its fence remains".into());
        }
    }
    for unit in [&config.daemon_unit, &config.edge_unit] {
        run_systemctl(runner, &config.systemctl, &["is-active", "--quiet", unit])?;
    }
    check_bindings()?;
    read_and_verify_manifest_owned(&manifest_path, runner, owner.0)?;
    if finalize::private(&pending_path, owner.0)? != pending {
        return Err("committed activation changed during recovery".into());
    }
    release_committed_fence(&config.runtime_dir, &pending, owner.0)?;
    end_drain(&drain)?;
    let receipt = RecoveryReceipt {
        status: "activated_awaiting_final_acceptance".into(),
        database_restored: false,
        snapshot: path_text(&config.transaction_dir.join("installation-snapshot.json"))?,
    };
    install::atomic_file(
        &config.transaction_dir.join("activation-recovery.json"),
        &serde_json::to_vec_pretty(&receipt)
            .map_err(|_| "cannot retain committed recovery receipt")?,
        0o600,
        Some(owner),
    )?;
    Ok(receipt)
}

pub(super) fn recover(
    config: &RecoveryConfig,
    snapshot: &InstallationSnapshot,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<RecoveryReceipt, String> {
    let archive = config.transaction_dir.join("authority-before.pgdump");
    if install::hash_file(&archive)? != snapshot.backup_sha256 {
        return Err("native recovery archive identity changed".into());
    }
    let path = config.transaction_dir.join("native-source.json");
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "native recovery binding is unavailable")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != owner.0
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("native recovery binding is not private and owned".into());
    }
    let context = read_json_file(&path)?.ok_or("native recovery binding is unavailable")?;
    let helper = PathBuf::from(
        context["helper_binary"]
            .as_str()
            .ok_or("native recovery has no verified helper")?,
    );
    if context["source_commit"].as_str() != Some(snapshot.candidate_commit.as_str())
        || context["helper_sha256"].as_str() != Some(install::hash_file(&helper)?.as_str())
    {
        return Err("native recovery helper no longer matches its frozen candidate".into());
    }
    let pg_config = PathBuf::from(
        context["postgres_config"]
            .as_str()
            .ok_or("native recovery has no private PostgreSQL configuration")?,
    );
    let pg_unit = context["postgres_unit"]
        .as_str()
        .filter(|name| name.ends_with(".service") && !name.is_empty() && !name.starts_with('-'))
        .ok_or("native recovery has no exact PostgreSQL service")?;
    install::validate_live_checkout(&config.source_root, false, runner).map_err(|error| {
        format!("Console source binding is unverified; native authority was preserved: {error}")
    })?;
    let drain = begin_drain(&config.runtime_dir, owner)?;
    startup_guard::apply_recovery(config, snapshot, runner, owner)?;
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
    run_systemctl(runner, &config.systemctl, &["start", pg_unit])?;
    let run = |mode: &str, report: &str, extra: Vec<OsString>| -> Result<(), String> {
        let report = config
            .transaction_dir
            .join(format!("{report}-{}.json", random_hex(6)?));
        let mut args = vec![
            "--mode".into(),
            mode.into(),
            "--database-config".into(),
            pg_config.as_os_str().to_owned(),
            "--report".into(),
            report.as_os_str().to_owned(),
        ];
        args.extend(extra);
        let result = runner.run(&CommandRequest {
            program: helper.clone(),
            args,
            environment: std::collections::BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            clear_environment: true,
        })?;
        if !result.success
            || result.stdout_truncated
            || result.stderr_truncated
            || read_json_file(&report)?
                .as_ref()
                .is_none_or(|value| value["verified"] != true)
        {
            return Err(
                "native recovery verification is incomplete; the prior database was preserved"
                    .into(),
            );
        }
        Ok(())
    };
    // The original database was never changed by candidate migrations. Refuse
    // any mismatch rather than restoring a stale archive over later data.
    run(
        "verify-backup",
        "native-recovery-source",
        vec!["--destination".into(), archive.into_os_string()],
    )?;
    restore_socket(&config.runtime_dir, &config.socket_path)?;
    restore_prior_edge_source(
        &config.transaction_dir,
        &config.source_root,
        &config.edge_env,
        Some(snapshot),
        runner,
        owner,
    )?;
    run_systemctl(runner, &config.systemctl, &["daemon-reload"])?;
    startup_guard::release_services(
        &config.transaction_dir,
        &config.daemon_unit,
        &config.edge_unit,
        &config.systemctl,
        runner,
        owner,
    )?;
    run_systemctl_all(
        runner,
        &config.systemctl,
        &[
            ("restart", &config.daemon_unit),
            ("restart", &config.edge_unit),
        ],
    )?;
    let state = config
        .database_path
        .parent()
        .ok_or("native recovery state has no parent")?;
    run(
        "verify-routes",
        "native-recovery-routes",
        vec![
            "--state-dir".into(),
            state.as_os_str().to_owned(),
            "--edge-state".into(),
            config.edge_state_dir.as_os_str().to_owned(),
        ],
    )?;
    end_drain(&drain)?;
    write_snapshot(snapshot, "recovered", owner)?;
    Ok(RecoveryReceipt {
        status: "recovered".into(),
        database_restored: false,
        snapshot: path_text(&config.transaction_dir.join("installation-snapshot.json"))?,
    })
}
