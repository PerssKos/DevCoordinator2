//! Transaction-owned start fencing for the Coordinator daemon and edge.
use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::DirBuilderExt;

const RECEIPT: &str = "daemon-start-guard.json";
const ACTIVE: &str = "daemon-start-guard.active";
const ACTIVE_BYTES: &[u8] = b"Coordinator cutover is incomplete\n";
const FREEZING: &str = "freezing-context.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FreezingContext {
    schema: u8,
    source_root: PathBuf,
    transaction: PathBuf,
    runtime: PathBuf,
    socket: PathBuf,
    database: PathBuf,
    daemon_unit: String,
    daemon_fragment: PathBuf,
    edge_unit: String,
    edge_fragment: PathBuf,
    edge_env: PathBuf,
    prior_manifest: PathBuf,
    native: bool,
    targets: Vec<PathBuf>,
    dropins: Vec<SnapshotEntry>,
    #[serde(default)]
    edge_dropins: Vec<SnapshotEntry>,
    native_sha256: Option<String>,
    edge_source_before_sha256: Option<String>,
}

pub(super) fn require_freezing(snapshot: &InstallationSnapshot) -> Result<(), String> {
    if snapshot.status != "freezing"
        || !snapshot.backup_sha256.is_empty()
        || snapshot.entries.len() != 12
        || snapshot.candidate_commit.len() != 40
        || !snapshot
            .candidate_commit
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err("freezing snapshot is not an unverified pre-installation binding".into());
    }
    Ok(())
}

pub(super) fn unchanged(snapshot: &InstallationSnapshot, owner: u32) -> Result<(), String> {
    let context: FreezingContext = serde_json::from_value(finalize::private(
        &Path::new(&snapshot.transaction_dir).join(FREEZING),
        owner,
    )?)
    .map_err(|_| "freezing context is invalid")?;
    if context.native_sha256.as_ref().is_some_and(|expected| {
        install::hash_file(&Path::new(&snapshot.transaction_dir).join("native-source.json"))
            .ok()
            .as_ref()
            != Some(expected)
    }) {
        return Err(
            "original native authority references changed; preserve the start guard".into(),
        );
    }
    for entry in snapshot
        .entries
        .iter()
        .chain(context.dropins.iter())
        .chain(context.edge_dropins.iter())
    {
        let current = capture_entry(Path::new(&entry.path))?;
        if serde_json::to_value(&current).map_err(|_| "cannot compare installation binding")?
            != serde_json::to_value(entry).map_err(|_| "cannot compare installation binding")?
        {
            return Err(
                "original static installation binding changed; preserve the start guard".into(),
            );
        }
    }
    Ok(())
}

/// Reload while guarded and compare the complete effective prior drop-in set.
pub(super) fn unchanged_effective(
    snapshot: &InstallationSnapshot,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    owner: u32,
) -> Result<(), String> {
    let transaction = Path::new(&snapshot.transaction_dir);
    let context: FreezingContext =
        serde_json::from_value(finalize::private(&transaction.join(FREEZING), owner)?)
            .map_err(|_| "freezing context is invalid")?;
    run_systemctl(runner, systemctl, &["daemon-reload"])?;
    require_active(transaction, &context.daemon_unit, runner, owner)?;
    let receipt = read(transaction, &context.daemon_unit, owner)?
        .ok_or("owned freezing guard is unavailable")?;
    let actual = if receipt.absent_source
        && require_stopped_state(systemctl, runner, &receipt.unit, true)?
    {
        std::collections::BTreeSet::new()
    } else {
        property(runner, &object(runner, &receipt.unit)?, "DropInPaths")?
            .as_array()
            .ok_or("effective drop-in bindings are unavailable")?
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_owned)
                    .ok_or("effective drop-in path is invalid")
            })
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?
    };
    let expected = context
        .dropins
        .iter()
        .map(|e| e.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let actual = actual
        .into_iter()
        .filter(|p| Path::new(p) != receipt.dropin)
        .collect::<std::collections::BTreeSet<_>>();
    if actual != expected {
        return Err("effective prior drop-in set changed; preserve foreign configuration and the start guard".into());
    }
    let edge_transaction = edge_transaction(transaction);
    require_active(&edge_transaction, &context.edge_unit, runner, owner)?;
    let receipt = read(&edge_transaction, &context.edge_unit, owner)?
        .ok_or("owned edge freezing guard is unavailable")?;
    let actual = if receipt.absent_source
        && require_stopped_state(systemctl, runner, &receipt.unit, true)?
    {
        std::collections::BTreeSet::new()
    } else {
        property(runner, &object(runner, &receipt.unit)?, "DropInPaths")?
            .as_array()
            .ok_or("effective edge drop-ins are unavailable")?
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_owned)
                    .ok_or("effective edge drop-in path is invalid")
            })
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?
    };
    let actual = actual
        .into_iter()
        .filter(|p| Path::new(p) != receipt.dropin)
        .collect::<std::collections::BTreeSet<_>>();
    let expected = context
        .edge_dropins
        .iter()
        .map(|e| e.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if actual != expected {
        return Err("effective prior edge drop-in set changed; preserve foreign configuration and both start guards".into());
    }
    unchanged(snapshot, owner)
}

/// Save static configuration before fencing starts; no live database is copied.
pub(super) fn capture_freezing(
    config: &HostCutoverConfig,
    manifest: &InstallManifest,
    native: bool,
    targets: &[&Path],
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    install::ensure_owned_directory(&config.transaction_dir, 0o700, owner)?;
    if exists(&config.transaction_dir.join("installation-snapshot.json"))? {
        let snapshot = read_snapshot(&config.transaction_dir, owner.0)?;
        require_freezing(&snapshot)?;
        return unchanged(&snapshot, owner.0);
    }
    if exists(&config.installed_manifest)? {
        install::read_and_verify_prior_manifest_owned(&config.installed_manifest, runner, owner.0)?;
    } else if exists(&config.daemon_unit_path)? {
        return Err("prior service has no verified source binding; preserve it".into());
    }
    let no_prior = !exists(&config.installed_manifest)? && !exists(&config.daemon_unit_path)?;
    let dropins = if no_prior {
        if !require_stopped_state(&config.systemctl, runner, &config.daemon_unit, true)? {
            return Err("unconfigured Coordinator unit name is already loaded".into());
        }
        Vec::new()
    } else {
        property(runner, &object(runner, &config.daemon_unit)?, "DropInPaths")?
            .as_array()
            .ok_or("original drop-in bindings are unavailable")?
            .iter()
            .map(|p| {
                p.as_str()
                    .ok_or_else(|| "original drop-in path is invalid".to_owned())
                    .and_then(|p| capture_entry(Path::new(p)))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let edge_dropins = if !exists(&config.edge_unit_path)?
        && require_stopped_state(&config.systemctl, runner, &config.edge_unit, true)?
    {
        Vec::new()
    } else {
        property(runner, &object(runner, &config.edge_unit)?, "DropInPaths")?
            .as_array()
            .ok_or("original edge drop-ins are unavailable")?
            .iter()
            .map(|p| {
                p.as_str()
                    .ok_or_else(|| "original edge drop-in path is invalid".to_owned())
                    .and_then(|p| capture_entry(Path::new(p)))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut context = FreezingContext {
        schema: 1,
        source_root: manifest.source_root.clone().into(),
        transaction: config.transaction_dir.clone(),
        runtime: config.runtime_dir.clone(),
        socket: config.socket_path.clone(),
        database: config.database_path.clone(),
        daemon_unit: config.daemon_unit.clone(),
        daemon_fragment: config.daemon_unit_path.clone(),
        edge_unit: config.edge_unit.clone(),
        edge_fragment: config.edge_unit_path.clone(),
        edge_env: config.edge_env.clone(),
        prior_manifest: config.installed_manifest.clone(),
        native,
        targets: targets.iter().map(|p| p.to_path_buf()).collect(),
        dropins,
        edge_dropins,
        native_sha256: None,
        edge_source_before_sha256: if exists(
            &config.transaction_dir.join("edge-source-before.json"),
        )? {
            Some(install::hash_file(
                &config.transaction_dir.join("edge-source-before.json"),
            )?)
        } else {
            None
        },
    };
    if native {
        let helper = manifest
            .binaries
            .iter()
            .find(|b| b.name == "devcoordinator2-migrate-authority")
            .ok_or("verified migration helper is unavailable")?;
        let source = serde_json::json!({"schema":1,"postgres_config":config.postgres.config_path,
            "postgres_unit":config.postgres.unit,"helper_binary":helper.path,"helper_sha256":helper.sha256,
            "source_commit":manifest.source_commit});
        install::atomic_file(
            &config.transaction_dir.join("native-source.json"),
            &serde_json::to_vec(&source).map_err(|_| "cannot retain native recovery binding")?,
            0o600,
            Some(owner),
        )?;
        context.native_sha256 = Some(install::hash_file(
            &config.transaction_dir.join("native-source.json"),
        )?);
    }
    install::atomic_file(
        &config.transaction_dir.join(FREEZING),
        &serde_json::to_vec(&context).map_err(|_| "cannot retain freezing context")?,
        0o600,
        Some(owner),
    )?;
    let snapshot = InstallationSnapshot {
        bootstrap: false,
        schema: if native { 3 } else { 2 },
        status: "freezing".into(),
        transaction_dir: path_text(&config.transaction_dir)?,
        entries: targets
            .iter()
            .map(|p| capture_entry(p))
            .collect::<Result<_, _>>()?,
        backup_sha256: String::new(),
        candidate_commit: manifest.source_commit.clone(),
    };
    require_freezing(&snapshot)?;
    write_snapshot(&snapshot, "freezing", owner)
}

/// Read-only canonical and pinned-source validation before any recovery mutation.
pub(super) fn validate_freezing_source(
    config: &RecoveryConfig,
    snapshot: &InstallationSnapshot,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    require_freezing(snapshot)?;
    let context: FreezingContext = serde_json::from_value(finalize::private(
        &config.transaction_dir.join(FREEZING),
        owner.0,
    )?)
    .map_err(|_| "freezing context is invalid")?;
    if context.transaction != config.transaction_dir
        || context.source_root != config.source_root
        || context.edge_env != config.edge_env
    {
        return Err("freezing source belongs to another canonical installation".into());
    }
    install::validate_live_checkout(&context.source_root, false, runner).map_err(|error| {
        format!(
            "freezing recovery source is unverified; bindings and guards were preserved: {error}"
        )
    })?;
    match context.edge_source_before_sha256 {
        Some(expected) => {
            if install::hash_file(&config.transaction_dir.join("edge-source-before.json"))?
                != expected
            {
                return Err(
                    "retained freezing edge-source binding changed; preserve both guards".into(),
                );
            }
            let binding = read_prior_edge_source(
                &config.transaction_dir,
                &context.source_root,
                &context.edge_env,
                Some(snapshot),
                runner,
                owner,
            )?;
            if binding.installed_manifest != context.prior_manifest
                || binding.edge_unit_path != context.edge_fragment
            {
                return Err(
                    "retained freezing source selected another installation binding".into(),
                );
            }
            install::edge_source::verify(&binding.source, runner)?;
        }
        None if exists(&context.prior_manifest)?
            || exists(&config.transaction_dir.join("edge-source-before.json"))? =>
        {
            return Err("freezing recovery has no captured prior edge-source identity".into());
        }
        None => {}
    }
    Ok(())
}

/// Resume only unchanged original bindings. An incomplete backup is never used.
pub(super) fn resume_freezing(
    config: &RecoveryConfig,
    snapshot: &InstallationSnapshot,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<RecoveryReceipt, String> {
    require_freezing(snapshot)?;
    let context: FreezingContext = serde_json::from_value(finalize::private(
        &config.transaction_dir.join(FREEZING),
        owner.0,
    )?)
    .map_err(|_| "freezing context is invalid")?;
    let expected = context
        .targets
        .iter()
        .map(|p| path_text(p))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    let actual = snapshot
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if context.schema != 1
        || context.transaction != config.transaction_dir
        || context.runtime != config.runtime_dir
        || context.socket != config.socket_path
        || context.database != config.database_path
        || context.daemon_unit != config.daemon_unit
        || context.edge_unit != config.edge_unit
        || context.edge_env != config.edge_env
        || context.source_root != config.source_root
        || context.native != (snapshot.schema == 3)
        || expected != actual
        || expected.len() != 12
        || !expected.contains(&path_text(&context.daemon_fragment)?)
        || !expected.contains(&path_text(&context.edge_fragment)?)
        || !expected.contains(&path_text(&context.prior_manifest)?)
    {
        return Err("freezing context belongs to another installation".into());
    }
    validate_freezing_source(config, snapshot, runner, owner)?;
    unchanged(snapshot, owner.0)?;
    let no_prior = !exists(&context.prior_manifest)? && !exists(&context.daemon_fragment)?;
    apply(
        &config.transaction_dir,
        &config.daemon_unit,
        Some(&context.daemon_fragment),
        no_prior,
        &config.systemctl,
        runner,
        owner,
    )?;
    apply_edge(
        &config.transaction_dir,
        &config.edge_unit,
        Some(&context.edge_fragment),
        &config.systemctl,
        runner,
        owner,
    )?;
    for (unit, path, optional) in [
        (&config.edge_unit, &context.edge_fragment, true),
        (&config.daemon_unit, &context.daemon_fragment, no_prior),
    ] {
        stop_bound_unit(&config.systemctl, runner, unit, path, optional)?;
    }
    unchanged_effective(snapshot, &config.systemctl, runner, owner.0)?;
    if context.native {
        let source =
            finalize::private(&config.transaction_dir.join("native-source.json"), owner.0)?;
        let helper = Path::new(
            source["helper_binary"]
                .as_str()
                .ok_or("freezing helper is unavailable")?,
        );
        let pg_config = Path::new(
            source["postgres_config"]
                .as_str()
                .ok_or("freezing authority is unavailable")?,
        );
        let pg_unit = source["postgres_unit"]
            .as_str()
            .ok_or("freezing PostgreSQL service is unavailable")?;
        exact_unit(pg_unit)?;
        if source["schema"] != 1
            || source["source_commit"] != snapshot.candidate_commit
            || source["helper_sha256"].as_str() != Some(install::hash_file(helper)?.as_str())
            || !expected.contains(&path_text(pg_config)?)
            || command(runner, helper, vec!["--source-commit".into()])?.trim()
                != snapshot.candidate_commit
        {
            return Err("freezing authority helper binding changed".into());
        }
        run_systemctl(runner, &config.systemctl, &["start", pg_unit])?;
        let report = config
            .transaction_dir
            .join(format!("freezing-authority-ready-{}.json", random_hex(8)?));
        command(
            runner,
            helper,
            vec![
                "--mode".into(),
                "inspect-postgres".into(),
                "--database-config".into(),
                pg_config.as_os_str().to_owned(),
                "--report".into(),
                report.as_os_str().to_owned(),
            ],
        )?;
        if finalize::private(&report, owner.0)?["verified"] != true {
            return Err("original PostgreSQL authority is not ready; start guard remains".into());
        }
    } else if exists(&config.database_path)? && !database_integrity(&config.database_path)? {
        return Err("original SQLite authority is not ready; start guard remains".into());
    }
    if exists(&context.prior_manifest)? {
        install::read_and_verify_prior_manifest_owned(&context.prior_manifest, runner, owner.0)?;
    }
    unchanged(snapshot, owner.0)?;
    unchanged_effective(snapshot, &config.systemctl, runner, owner.0)?;
    validate_freezing_source(config, snapshot, runner, owner)?;
    restore_socket(&config.runtime_dir, &config.socket_path)?;
    release_services(
        &config.transaction_dir,
        &config.daemon_unit,
        &config.edge_unit,
        &config.systemctl,
        runner,
        owner,
    )?;
    for (unit, path) in [
        (&config.daemon_unit, &context.daemon_fragment),
        (&config.edge_unit, &context.edge_fragment),
    ] {
        if exists(path)? {
            run_systemctl(runner, &config.systemctl, &["restart", unit])?;
            run_systemctl(runner, &config.systemctl, &["is-active", "--quiet", unit])?;
        }
    }
    write_snapshot(snapshot, "recovered", owner)?;
    Ok(RecoveryReceipt {
        status: "recovered".into(),
        database_restored: false,
        snapshot: path_text(&config.transaction_dir.join("installation-snapshot.json"))?,
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u8,
    unit: String,
    transaction: PathBuf,
    fragment: PathBuf,
    dropin: PathBuf,
    dropin_sha256: String,
    active: PathBuf,
    released: bool,
    #[serde(default)]
    absent_source: bool,
}

fn command(
    runner: &dyn CommandRunner,
    program: &Path,
    args: Vec<OsString>,
) -> Result<String, String> {
    let output = runner.run(&CommandRequest {
        program: program.to_owned(),
        args,
        environment: std::collections::BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from("/usr/bin:/bin"),
        )]),
        clear_environment: true,
    })?;
    if !output.success || output.stdout_truncated || output.stderr_truncated {
        return Err("Coordinator start-guard systemd observation failed".into());
    }
    Ok(output.stdout)
}

fn object(runner: &dyn CommandRunner, unit: &str) -> Result<String, String> {
    let output = command(
        runner,
        Path::new("/usr/bin/busctl"),
        [
            "--json=short",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "LoadUnit",
            "s",
            unit,
        ]
        .map(OsString::from)
        .to_vec(),
    )?;
    serde_json::from_str::<Value>(&output)
        .ok()
        .and_then(|v| v["data"][0].as_str().map(str::to_owned))
        .filter(|p| p.starts_with("/org/freedesktop/systemd1/unit/"))
        .ok_or_else(|| "Coordinator start-guard unit identity is unavailable".into())
}

fn property(runner: &dyn CommandRunner, object: &str, name: &str) -> Result<Value, String> {
    let output = command(
        runner,
        Path::new("/usr/bin/busctl"),
        [
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            object,
            "org.freedesktop.systemd1.Unit",
            name,
        ]
        .map(OsString::from)
        .to_vec(),
    )?;
    serde_json::from_str::<Value>(&output)
        .map(|v| v["data"].clone())
        .map_err(|_| "Coordinator start-guard property is invalid".into())
}

fn exact_unit(unit: &str) -> Result<(), String> {
    if !unit.ends_with(".service")
        || unit.len() > 128
        || !unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'))
    {
        return Err("Coordinator start guard requires its exact service unit".into());
    }
    Ok(())
}

fn owned(path: &Path, owner: u32, private: bool) -> Result<(), String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "Coordinator start-guard file is unavailable")?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != owner
        || private && metadata.mode() & 0o077 != 0
        || metadata.len() > 64 * 1024
    {
        return Err("Coordinator start-guard file ownership or type changed".into());
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool, String> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("cannot inspect Coordinator start-guard entry".into()),
    }
}

fn dropin_path(transaction: &Path, unit: &str, fragment: &Path) -> Result<PathBuf, String> {
    let hash = Sha256::digest(path_text(transaction)?.as_bytes());
    let suffix = hash[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let parent = fragment
        .parent()
        .ok_or("Coordinator service has no unit directory")?;
    Ok(parent
        .join(format!("{unit}.d"))
        .join(format!("90-devcoordinator2-cutover-freeze-{suffix}.conf")))
}

fn read(transaction: &Path, unit: &str, owner: u32) -> Result<Option<Receipt>, String> {
    let path = transaction.join(RECEIPT);
    if path
        .symlink_metadata()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(None);
    }
    owned(&path, owner, true)?;
    let value: Receipt = serde_json::from_slice(
        &std::fs::read(&path).map_err(|_| "cannot read Coordinator start guard")?,
    )
    .map_err(|_| "Coordinator start-guard receipt is invalid")?;
    if value.schema != 1
        || value.unit != unit
        || value.transaction != transaction
        || value.active != transaction.join(ACTIVE)
    {
        return Err("Coordinator start guard belongs to another transaction".into());
    }
    exact_unit(unit)?;
    let expected_hash = Sha256::digest(condition(&value.active)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if !value.fragment.is_absolute()
        || value.dropin != dropin_path(transaction, unit, &value.fragment)?
        || value.dropin_sha256 != expected_hash
    {
        return Err("Coordinator start-guard receipt binding is invalid".into());
    }
    Ok(Some(value))
}

fn condition(active: &Path) -> Result<Vec<u8>, String> {
    let text = path_text(active)?;
    if !active.is_absolute()
        || text
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'%' | b'\\' | b'\'' | b'"'))
    {
        return Err("Coordinator start guard requires an exact absolute condition path".into());
    }
    Ok(format!("[Unit]\nConditionPathExists=!{text}\n").into_bytes())
}

fn effective(receipt: &Receipt, runner: &dyn CommandRunner) -> Result<(), String> {
    if receipt.absent_source
        && require_stopped_state(Path::new("/usr/bin/systemctl"), runner, &receipt.unit, true)?
    {
        return Ok(());
    }
    let object = object(runner, &receipt.unit)?;
    if property(runner, &object, "FragmentPath")?.as_str() != receipt.fragment.to_str() {
        return Err("Coordinator service fragment changed while guarded".into());
    }
    let dropins = property(runner, &object, "DropInPaths")?;
    if !dropins
        .as_array()
        .is_some_and(|rows| rows.iter().any(|p| p.as_str() == receipt.dropin.to_str()))
    {
        return Err("Coordinator start-guard drop-in is not effective".into());
    }
    let conditions = property(runner, &object, "Conditions")?;
    if !conditions.as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row[0] == "ConditionPathExists"
                && row[1] == false
                && row[2] == true
                && row[3].as_str() == receipt.active.to_str()
        })
    }) {
        return Err("Coordinator start-guard condition was shadowed or reset".into());
    }
    Ok(())
}

/// Apply/reconcile the same persisted guard; process death never releases it.
pub(super) fn apply(
    transaction: &Path,
    unit: &str,
    expected_fragment: Option<&Path>,
    allow_absent: bool,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    exact_unit(unit)?;
    install::ensure_owned_directory(transaction, 0o700, owner)?;
    let previous = read(transaction, unit, owner.0)?;
    let intended = expected_fragment
        .map(Path::to_path_buf)
        .or_else(|| previous.as_ref().map(|p| p.fragment.clone()));
    let intended_missing = match &intended {
        Some(p) => !exists(p)?,
        None => false,
    };
    let absent_source = (allow_absent || previous.as_ref().is_some_and(|p| p.absent_source))
        && intended_missing
        && require_stopped_state(systemctl, runner, unit, true)?;
    let fragment = if absent_source {
        intended.ok_or("absent Coordinator source has no prior unit binding")?
    } else {
        property(runner, &object(runner, unit)?, "FragmentPath")?
            .as_str()
            .map(PathBuf::from)
            .ok_or("Coordinator service fragment is unavailable")?
    };
    if !fragment.is_absolute() || expected_fragment.is_some_and(|expected| expected != fragment) {
        return Err("Coordinator start guard selected another service fragment".into());
    }
    if absent_source {
        if exists(&fragment)? {
            return Err("Coordinator fragment is present but not loaded".into());
        }
        require_stopped_state(systemctl, runner, unit, true)?;
    } else {
        owned(&fragment, owner.0, false)?;
    }
    let dropin = dropin_path(transaction, unit, &fragment)?;
    let active = transaction.join(ACTIVE);
    let bytes = condition(&active)?;
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if let Some(previous) = &previous {
        if previous.fragment != fragment
            || previous.dropin != dropin
            || previous.dropin_sha256 != sha256
        {
            return Err("Coordinator start-guard binding changed".into());
        }
    } else if exists(&dropin)? || exists(&active)? {
        return Err("Coordinator start guard would overwrite unknown configuration".into());
    }
    let receipt = Receipt {
        schema: 1,
        unit: unit.into(),
        transaction: transaction.into(),
        fragment,
        dropin,
        dropin_sha256: sha256,
        active,
        released: false,
        absent_source,
    };
    // Persist ownership before publishing either file so interrupted setup is repairable.
    install::atomic_file(
        &transaction.join(RECEIPT),
        &serde_json::to_vec(&receipt).map_err(|_| "cannot encode start guard")?,
        0o600,
        Some(owner),
    )?;
    if exists(&receipt.active)? {
        owned(&receipt.active, owner.0, true)?;
        if std::fs::read(&receipt.active).map_err(|_| "cannot read active guard")? != ACTIVE_BYTES {
            return Err("active Coordinator guard changed".into());
        }
    } else {
        install::atomic_file(&receipt.active, ACTIVE_BYTES, 0o600, Some(owner))?;
    }
    let directory = receipt.dropin.parent().unwrap();
    match std::fs::DirBuilder::new().mode(0o755).create(directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let m = directory
                .symlink_metadata()
                .map_err(|_| "cannot inspect guard directory")?;
            if !m.is_dir() || m.file_type().is_symlink() || m.uid() != owner.0 {
                return Err("Coordinator guard directory is not owned".into());
            }
        }
        Err(_) => return Err("cannot create Coordinator guard directory".into()),
    }
    if exists(&receipt.dropin)? {
        owned(&receipt.dropin, owner.0, false)?;
        if install::hash_file(&receipt.dropin)? != receipt.dropin_sha256 {
            return Err("Coordinator guard configuration changed".into());
        }
    } else {
        install::atomic_file(&receipt.dropin, &bytes, 0o644, Some(owner))?;
    }
    run_systemctl(runner, systemctl, &["daemon-reload"])?;
    effective(&receipt, runner)?;
    Ok(())
}

pub(super) fn require_active(
    transaction: &Path,
    unit: &str,
    runner: &dyn CommandRunner,
    owner: u32,
) -> Result<(), String> {
    let receipt =
        read(transaction, unit, owner)?.ok_or("Coordinator frozen window has no start guard")?;
    if receipt.released {
        return Err("Coordinator frozen window start guard was released".into());
    }
    owned(&receipt.active, owner, true)?;
    owned(&receipt.dropin, owner, false)?;
    if std::fs::read(&receipt.active).map_err(|_| "cannot read active guard")? != ACTIVE_BYTES
        || install::hash_file(&receipt.dropin)? != receipt.dropin_sha256
    {
        return Err("Coordinator frozen window guard changed".into());
    }
    effective(&receipt, runner)
}

pub(super) fn require_stopped(
    systemctl: &Path,
    runner: &dyn CommandRunner,
    unit: &str,
) -> Result<(), String> {
    require_stopped_state(systemctl, runner, unit, false).map(|_| ())
}

fn require_stopped_state(
    systemctl: &Path,
    runner: &dyn CommandRunner,
    unit: &str,
    allow_absent: bool,
) -> Result<bool, String> {
    let output = command(
        runner,
        systemctl,
        [
            "show",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=MainPID",
            unit,
        ]
        .map(OsString::from)
        .to_vec(),
    )?;
    let values = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<std::collections::BTreeMap<_, _>>();
    if !(values.get("LoadState") == Some(&"loaded")
        || allow_absent && values.get("LoadState") == Some(&"not-found"))
        || values.get("MainPID") != Some(&"0")
        || !matches!(
            values.get("ActiveState"),
            Some(&"inactive") | Some(&"failed")
        )
    {
        return Err("Coordinator frozen window requires an inactive service and MainPID=0".into());
    }
    Ok(values.get("LoadState") == Some(&"not-found"))
}

pub(super) fn require_source_stopped(
    transaction: &Path,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    unit: &str,
    owner: u32,
) -> Result<(), String> {
    let value = read(transaction, unit, owner)?.ok_or("Coordinator source has no guard receipt")?;
    require_stopped_state(systemctl, runner, unit, value.absent_source).map(|_| ())
}

pub(super) fn stop_services(
    config: &HostCutoverConfig,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    let no_prior = !exists(&config.installed_manifest)? && !exists(&config.daemon_unit_path)?;
    apply(
        &config.transaction_dir,
        &config.daemon_unit,
        Some(&config.daemon_unit_path),
        no_prior,
        &config.systemctl,
        runner,
        owner,
    )?;
    apply_edge(
        &config.transaction_dir,
        &config.edge_unit,
        Some(&config.edge_unit_path),
        &config.systemctl,
        runner,
        owner,
    )?;
    for (unit, path, optional) in [
        (&config.edge_unit, &config.edge_unit_path, true),
        (&config.daemon_unit, &config.daemon_unit_path, no_prior),
    ] {
        stop_bound_unit(&config.systemctl, runner, unit, path, optional)?;
    }
    Ok(())
}

pub(super) fn edge_transaction(transaction: &Path) -> PathBuf {
    transaction.join("edge-start-guard")
}

fn apply_edge(
    transaction: &Path,
    unit: &str,
    fragment: Option<&Path>,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    let absent = match fragment {
        Some(path) => !exists(path)?,
        None => false,
    };
    apply(
        &edge_transaction(transaction),
        unit,
        fragment,
        absent,
        systemctl,
        runner,
        owner,
    )
}

pub(super) fn apply_recovery(
    config: &RecoveryConfig,
    snapshot: &InstallationSnapshot,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    apply(
        &config.transaction_dir,
        &config.daemon_unit,
        None,
        false,
        &config.systemctl,
        runner,
        owner,
    )?;
    let edge = snapshot
        .entries
        .iter()
        .find(|entry| {
            Path::new(&entry.path).file_name() == Some(std::ffi::OsStr::new(&config.edge_unit))
        })
        .ok_or("recovery has no exact edge fragment binding")?;
    apply_edge(
        &config.transaction_dir,
        &config.edge_unit,
        Some(Path::new(&edge.path)),
        &config.systemctl,
        runner,
        owner,
    )
}

pub(super) fn release_services(
    transaction: &Path,
    daemon: &str,
    edge: &str,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    preflight_release(transaction, daemon, owner.0)?;
    preflight_release(&edge_transaction(transaction), edge, owner.0)?;
    release(transaction, daemon, systemctl, runner, owner)?;
    release(
        &edge_transaction(transaction),
        edge,
        systemctl,
        runner,
        owner,
    )
}

fn stop_bound_unit(
    systemctl: &Path,
    runner: &dyn CommandRunner,
    unit: &str,
    path: &Path,
    optional: bool,
) -> Result<(), String> {
    if optional && !exists(path)? && require_stopped_state(systemctl, runner, unit, true)? {
        return Ok(());
    }
    if property(runner, &object(runner, unit)?, "FragmentPath")?.as_str() != path.to_str() {
        return Err("frozen service selected another unit fragment; preserve it".into());
    }
    run_systemctl(runner, systemctl, &["stop", unit])?;
    require_stopped(systemctl, runner, unit)
}

/// Release only after the caller has restored or prepared complete bindings.
pub(super) fn release(
    transaction: &Path,
    unit: &str,
    systemctl: &Path,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<(), String> {
    let Some(mut receipt) = preflight_release(transaction, unit, owner.0)? else {
        return Ok(());
    };
    let has_dropin = exists(&receipt.dropin)?;
    let has_active = exists(&receipt.active)?;
    if has_dropin {
        std::fs::remove_file(&receipt.dropin)
            .map_err(|_| "cannot release owned Coordinator condition")?;
    }
    run_systemctl(runner, systemctl, &["daemon-reload"])?;
    if has_active {
        std::fs::remove_file(&receipt.active)
            .map_err(|_| "cannot release active Coordinator guard")?;
    }
    receipt.released = true;
    install::atomic_file(
        &transaction.join(RECEIPT),
        &serde_json::to_vec(&receipt).map_err(|_| "cannot encode released guard")?,
        0o600,
        Some(owner),
    )
}

fn preflight_release(
    transaction: &Path,
    unit: &str,
    owner: u32,
) -> Result<Option<Receipt>, String> {
    let Some(receipt) = read(transaction, unit, owner)? else {
        return Ok(None);
    };
    let has_dropin = exists(&receipt.dropin)?;
    let has_active = exists(&receipt.active)?;
    // Check every component of both services before releasing either one.
    if has_dropin {
        owned(&receipt.dropin, owner, false)?;
        if install::hash_file(&receipt.dropin)? != receipt.dropin_sha256 {
            return Err("Coordinator guard configuration changed; preserve it".into());
        }
    }
    if has_active {
        owned(&receipt.active, owner, true)?;
        if std::fs::read(&receipt.active).map_err(|_| "cannot read active guard")? != ACTIVE_BYTES {
            return Err("active Coordinator guard changed".into());
        }
    }
    Ok(Some(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Systemd {
        fragment: PathBuf,
        state: Mutex<(String, String, u64)>,
        fail: bool,
        reject_load: bool,
    }
    impl CommandRunner for Systemd {
        fn run(&self, request: &CommandRequest) -> Result<install::CommandOutput, String> {
            let args = request
                .args
                .iter()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if self.reject_load && args.iter().any(|a| a == "LoadUnit") {
                return Err("missing unit cannot be loaded".into());
            }
            let state = self.state.lock().unwrap();
            let value = if request.program.file_name() == Some(std::ffi::OsStr::new("busctl")) {
                if args.iter().any(|v| v == "LoadUnit") {
                    serde_json::json!({"type":"o","data":["/org/freedesktop/systemd1/unit/fixture"]})
                } else {
                    let name = args.last().unwrap();
                    let directory = self.fragment.parent().unwrap().join(format!(
                        "{}.d",
                        self.fragment.file_name().unwrap().to_string_lossy()
                    ));
                    let mut paths = if directory.is_dir() {
                        std::fs::read_dir(directory)
                            .unwrap()
                            .map(|v| v.unwrap().path())
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    paths.sort();
                    let data = match name.as_str() {
                        "FragmentPath" => {
                            if state.0 == "not-found" {
                                serde_json::json!("")
                            } else {
                                serde_json::json!(self.fragment)
                            }
                        }
                        "LoadState" => serde_json::json!(state.0),
                        "DropInPaths" => serde_json::json!(paths),
                        "Conditions" => {
                            let mut conditions = Vec::<Value>::new();
                            for path in paths {
                                if let Ok(text) = std::fs::read_to_string(path) {
                                    for line in text.lines() {
                                        if let Some(value) =
                                            line.strip_prefix("ConditionPathExists=")
                                        {
                                            if value.is_empty() {
                                                conditions.clear();
                                            } else {
                                                conditions.push(serde_json::json!([
                                                    "ConditionPathExists",
                                                    false,
                                                    value.starts_with('!'),
                                                    value.trim_start_matches('!'),
                                                    0
                                                ]));
                                            }
                                        }
                                    }
                                }
                            }
                            serde_json::json!(conditions)
                        }
                        _ => Value::Null,
                    };
                    serde_json::json!({"data":data})
                }
            } else if args.first().is_some_and(|v| v == "show") {
                return Ok(install::CommandOutput {
                    success: !self.fail,
                    stdout: format!(
                        "LoadState={}\nActiveState={}\nMainPID={}\n",
                        state.0, state.1, state.2
                    ),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                });
            } else {
                Value::Null
            };
            Ok(install::CommandOutput {
                success: !self.fail,
                stdout: if value.is_null() {
                    String::new()
                } else {
                    value.to_string()
                },
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }
    fn setup() -> (tempfile::TempDir, Systemd, PathBuf, (u32, u32)) {
        let root = tempfile::tempdir().unwrap();
        let fragment = root.path().join("fixture.service");
        std::fs::write(&fragment, "operator unit\n").unwrap();
        let tx = root.path().join("transaction");
        let owner = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let systemd = Systemd {
            fragment,
            state: Mutex::new(("loaded".into(), "inactive".into(), 0)),
            fail: false,
            reject_load: false,
        };
        (root, systemd, tx, owner)
    }
    fn guard(systemd: &Systemd, tx: &Path, owner: (u32, u32)) -> Result<(), String> {
        apply(
            tx,
            "fixture.service",
            Some(&systemd.fragment),
            false,
            Path::new("systemctl"),
            systemd,
            owner,
        )
    }
    #[test]
    fn guard_reconciles_and_preserves_existing_configuration_until_explicit_release() {
        let (_root, systemd, tx, owner) = setup();
        let directory = systemd.fragment.parent().unwrap().join("fixture.service.d");
        std::fs::create_dir(&directory).unwrap();
        let operator = directory.join("10-operator.conf");
        std::fs::write(&operator, "[Unit]\nConditionPathExists=/\n").unwrap();
        guard(&systemd, &tx, owner).unwrap();
        guard(&systemd, &tx, owner).unwrap();
        require_active(&tx, "fixture.service", &systemd, owner.0).unwrap();
        require_stopped(Path::new("systemctl"), &systemd, "fixture.service").unwrap();
        {
            let mut state = systemd.state.lock().unwrap();
            *state = ("loaded".into(), "active".into(), 17);
        }
        assert!(require_stopped(Path::new("systemctl"), &systemd, "fixture.service").is_err());
        {
            let mut state = systemd.state.lock().unwrap();
            *state = ("not-found".into(), "inactive".into(), 0);
        }
        assert!(require_stopped(Path::new("systemctl"), &systemd, "fixture.service").is_err());
        release(
            &tx,
            "fixture.service",
            Path::new("systemctl"),
            &systemd,
            owner,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(operator).unwrap(),
            "[Unit]\nConditionPathExists=/\n"
        );
        assert_eq!(
            std::fs::read_to_string(&systemd.fragment).unwrap(),
            "operator unit\n"
        );
        assert!(
            read(&tx, "fixture.service", owner.0)
                .unwrap()
                .unwrap()
                .released
        );
        // A configured inactive unit remains valid; disappearance of that expected
        // unit is an error. Only an explicitly empty initial installation may
        // prepare a guard beside the future fragment.
        assert!(guard(&systemd, &tx, owner).is_err());
        let (_root, mut absent, tx, owner) = setup();
        std::fs::remove_file(&absent.fragment).unwrap();
        *absent.state.lock().unwrap() = ("not-found".into(), "inactive".into(), 0);
        absent.reject_load = true;
        assert!(guard(&absent, &tx, owner).is_err());
        apply(
            &tx,
            "fixture.service",
            Some(&absent.fragment),
            true,
            Path::new("systemctl"),
            &absent,
            owner,
        )
        .unwrap();
        require_source_stopped(
            &tx,
            Path::new("systemctl"),
            &absent,
            "fixture.service",
            owner.0,
        )
        .unwrap();
        std::fs::write(&absent.fragment, "new owned unit\n").unwrap();
        *absent.state.lock().unwrap() = ("loaded".into(), "inactive".into(), 0);
        absent.reject_load = false;
        require_active(&tx, "fixture.service", &absent, owner.0).unwrap();
        release(
            &tx,
            "fixture.service",
            Path::new("systemctl"),
            &absent,
            owner,
        )
        .unwrap();
    }
    #[test]
    fn changed_active_marker_never_unblocks_starts() {
        let (_root, systemd, tx, owner) = setup();
        guard(&systemd, &tx, owner).unwrap();
        let receipt = read(&tx, "fixture.service", owner.0).unwrap().unwrap();
        std::fs::write(&receipt.active, "changed").unwrap();
        assert!(
            release(
                &tx,
                "fixture.service",
                Path::new("systemctl"),
                &systemd,
                owner
            )
            .is_err()
        );
        assert!(receipt.dropin.is_file());
        assert_eq!(
            install::hash_file(&receipt.dropin).unwrap(),
            receipt.dropin_sha256
        );
        std::fs::write(&receipt.active, ACTIVE_BYTES).unwrap();
        std::fs::write(&receipt.dropin, "foreign replacement\n").unwrap();
        assert!(
            release(
                &tx,
                "fixture.service",
                Path::new("systemctl"),
                &systemd,
                owner
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&receipt.active).unwrap(), ACTIVE_BYTES);
        assert_eq!(
            std::fs::read_to_string(&receipt.dropin).unwrap(),
            "foreign replacement\n"
        );
    }
    #[test]
    fn paired_release_preflights_foreign_edge_state_before_unblocking_daemon() {
        for fault in ["marker", "dropin", "receipt"] {
            let (_root, systemd, tx, owner) = setup();
            let (_edge_root, mut edge, _, _) = setup();
            let edge_fragment = edge.fragment.with_file_name("edge.service");
            std::fs::rename(&edge.fragment, &edge_fragment).unwrap();
            edge.fragment = edge_fragment;
            guard(&systemd, &tx, owner).unwrap();
            let edge_tx = edge_transaction(&tx);
            apply(
                &edge_tx,
                "edge.service",
                Some(&edge.fragment),
                false,
                Path::new("systemctl"),
                &edge,
                owner,
            )
            .unwrap();
            let daemon = read(&tx, "fixture.service", owner.0).unwrap().unwrap();
            let edge = read(&edge_tx, "edge.service", owner.0).unwrap().unwrap();
            let before = [
                (&daemon.active, std::fs::read(&daemon.active).unwrap()),
                (&daemon.dropin, std::fs::read(&daemon.dropin).unwrap()),
                (&tx.join(RECEIPT), std::fs::read(tx.join(RECEIPT)).unwrap()),
            ];
            let changed = match fault {
                "marker" => edge.active,
                "dropin" => edge.dropin,
                _ => edge_tx.join(RECEIPT),
            };
            std::fs::write(&changed, b"foreign edge state must be preserved").unwrap();
            assert!(
                release_services(
                    &tx,
                    "fixture.service",
                    "edge.service",
                    Path::new("systemctl"),
                    &systemd,
                    owner
                )
                .is_err()
            );
            for (file, bytes) in before {
                assert_eq!(std::fs::read(file).unwrap(), bytes, "{fault}");
            }
            assert_eq!(
                std::fs::read(changed).unwrap(),
                b"foreign edge state must be preserved"
            );
            require_active(&tx, "fixture.service", &systemd, owner.0).unwrap();
        }
    }
    #[test]
    fn dangling_entries_and_unknown_collisions_are_preserved() {
        for component in [ACTIVE, "dropin"] {
            let (_root, systemd, tx, owner) = setup();
            guard(&systemd, &tx, owner).unwrap();
            let receipt = read(&tx, "fixture.service", owner.0).unwrap().unwrap();
            let path = if component == ACTIVE {
                receipt.active
            } else {
                receipt.dropin
            };
            std::fs::remove_file(&path).unwrap();
            symlink("missing-owned-target", &path).unwrap();
            assert!(guard(&systemd, &tx, owner).is_err());
            assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
            assert!(
                release(
                    &tx,
                    "fixture.service",
                    Path::new("systemctl"),
                    &systemd,
                    owner
                )
                .is_err()
            );
            assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        }
        let (_root, systemd, tx, owner) = setup();
        let path = dropin_path(&tx, "fixture.service", &systemd.fragment).unwrap();
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "foreign operator file").unwrap();
        assert!(guard(&systemd, &tx, owner).is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "foreign operator file"
        );
    }
    #[test]
    fn corrupt_receipt_and_effective_condition_reset_fail_closed() {
        let (_root, systemd, tx, owner) = setup();
        guard(&systemd, &tx, owner).unwrap();
        let mut receipt = read(&tx, "fixture.service", owner.0).unwrap().unwrap();
        let real = receipt.dropin.clone();
        receipt.dropin = systemd.fragment.clone();
        install::atomic_file(
            &tx.join(RECEIPT),
            &serde_json::to_vec(&receipt).unwrap(),
            0o600,
            Some(owner),
        )
        .unwrap();
        assert!(
            release(
                &tx,
                "fixture.service",
                Path::new("systemctl"),
                &systemd,
                owner
            )
            .is_err()
        );
        assert!(real.is_file());
        assert_eq!(
            std::fs::read_to_string(&systemd.fragment).unwrap(),
            "operator unit\n"
        );
        let (_root, systemd, tx, owner) = setup();
        let directory = systemd.fragment.parent().unwrap().join("fixture.service.d");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("zz-reset.conf"),
            "[Unit]\nConditionPathExists=\n",
        )
        .unwrap();
        assert!(guard(&systemd, &tx, owner).is_err());
        assert!(tx.join(ACTIVE).is_file());
        let (_root, mut systemd, tx, owner) = setup();
        systemd.fail = true;
        assert!(guard(&systemd, &tx, owner).is_err());
        assert!(require_stopped(Path::new("systemctl"), &systemd, "fixture.service").is_err());
    }
}
