//! Retirement is a second, explicit boundary after new live writes are open.
use super::{CommandRequest, CommandRunner, random_hex, read_json_file, read_snapshot};
use crate::install::{self, InstallManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const LIVE_CHECKS: &[&str] = &[
    "browser",
    "edge-source",
    "critical-journeys",
    "critical-api",
    "mcp-reconnect",
];

#[derive(Clone, Debug)]
pub struct FinalizationConfig {
    pub transaction_dir: PathBuf,
    pub installed_manifest: PathBuf,
    pub live_acceptance: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiveAcceptance {
    pub schema: u8,
    pub result: String,
    pub source_commit: String,
    pub edge_source_commit: String,
    pub binaries: BTreeMap<String, String>,
    pub checked_at_ms: u64,
    pub checks: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinalizationReceipt {
    pub schema: u8,
    pub status: String,
    pub source_commit: String,
    pub live_acceptance_sha256: String,
    pub postgres_archive_sha256: String,
    pub retired_sources: u8,
}

pub(super) fn private(path: &Path, owner: u32) -> Result<Value, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "required private finalization evidence is unavailable")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("finalization evidence must be an owned mode-0600 regular file".into());
    }
    read_json_file(path)?.ok_or_else(|| "required finalization evidence is unavailable".into())
}

fn check_live(
    path: &Path,
    manifest: &InstallManifest,
    activated_at_ms: u64,
    owner: u32,
) -> Result<String, String> {
    let value = private(path, owner)?;
    let receipt: LiveAcceptance =
        serde_json::from_value(value).map_err(|_| "live acceptance receipt is invalid")?;
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).max(0) as u64;
    let checks = receipt
        .checks
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if receipt.schema != 1
        || receipt.result != "passed"
        || receipt.source_commit != manifest.source_commit
        || receipt.edge_source_commit != manifest.source_commit
        || receipt.binaries.len() != manifest.binaries.len()
        || manifest
            .binaries
            .iter()
            .any(|binary| receipt.binaries.get(&binary.name) != Some(&binary.sha256))
        || receipt.checked_at_ms < activated_at_ms
        || receipt.checked_at_ms > now
        || LIVE_CHECKS.iter().any(|check| !checks.contains(check))
        || receipt.evidence.is_empty()
        || receipt
            .evidence
            .iter()
            .any(|e| e.is_empty() || e.len() > 4096)
    {
        return Err("fresh post-activation browser, edge, journey, API and reconnected-client evidence must match the installed binaries".into());
    }
    install::hash_file(path)
}

fn helper(
    runner: &dyn CommandRunner,
    binary: &Path,
    mode: &str,
    config: &Path,
    archive: &Path,
    report: &Path,
    owner: u32,
) -> Result<Value, String> {
    let output = runner.run(&CommandRequest {
        program: binary.to_owned(),
        args: vec![
            "--mode".into(),
            mode.into(),
            "--database-config".into(),
            config.as_os_str().to_owned(),
            "--destination".into(),
            archive.as_os_str().to_owned(),
            "--report".into(),
            report.as_os_str().to_owned(),
        ],
        environment: BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))]),
        clear_environment: true,
    })?;
    if !output.success || output.stdout_truncated || output.stderr_truncated {
        return Err("new live PostgreSQL backup recovery verification failed; all legacy sources remain protected".into());
    }
    let receipt = private(report, owner)?;
    if receipt["verified"] != true
        || receipt["archive_sha256"].as_str() != Some(install::hash_file(archive)?.as_str())
    {
        return Err(
            "fresh PostgreSQL backup evidence does not match the retained native archive".into(),
        );
    }
    Ok(receipt)
}

fn finish_protection_marker(
    transaction: &Path,
    receipt: &FinalizationReceipt,
    owner: u32,
) -> Result<(), String> {
    let marker = transaction.join("retirement-pending.json");
    match marker.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("cannot inspect finalization protection marker".into()),
        Ok(_) => {}
    }
    let pending = private(&marker, owner)?;
    if pending["source_commit"] != receipt.source_commit {
        return Err("finalization protection marker belongs to another candidate".into());
    }
    let targets = pending["retirement_targets"]
        .as_array()
        .filter(|targets| targets.len() <= 32)
        .ok_or("finalization protection marker has invalid retirement targets")?;
    for target in targets {
        let path = Path::new(
            target["path"]
                .as_str()
                .ok_or("invalid retired source path")?,
        );
        if !path.is_absolute() {
            return Err("invalid retired source path".into());
        }
        match path.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(
                    "finalized source retirement is inconsistent; protection remains".into(),
                );
            }
        }
    }
    std::fs::remove_file(&marker)
        .map_err(|_| "sources retired but finalization protection marker remains")?;
    std::fs::File::open(transaction)
        .and_then(|file| file.sync_all())
        .map_err(|_| "finalization directory could not be sealed")?;
    Ok(())
}

pub fn finalize_host(
    config: &FinalizationConfig,
    runner: &dyn CommandRunner,
    owner: (u32, u32),
) -> Result<FinalizationReceipt, String> {
    let result_path = config.transaction_dir.join("finalization.json");
    if result_path.exists() {
        let value = private(&result_path, owner.0)?;
        if value["status"] == "finalized" {
            let receipt = serde_json::from_value(value)
                .map_err(|_| "saved finalization receipt is invalid")?;
            finish_protection_marker(&config.transaction_dir, &receipt, owner.0)?;
            return Ok(receipt);
        }
    }
    let snapshot = read_snapshot(&config.transaction_dir, owner.0)?;
    if snapshot.schema != 3 || snapshot.status != "committed" {
        return Err("finalization requires an admitted PostgreSQL activation".into());
    }
    let pending = private(
        &config.transaction_dir.join("retirement-pending.json"),
        owner.0,
    )?;
    let manifest =
        install::read_and_verify_manifest_owned(&config.installed_manifest, runner, owner.0)?;
    if manifest.source_commit != snapshot.candidate_commit
        || pending["source_commit"].as_str() != Some(manifest.source_commit.as_str())
    {
        return Err("finalization belongs to another installed candidate".into());
    }
    let current = install::validate_live_checkout(Path::new(&manifest.source_root), false, runner)?;
    if current != manifest.source_commit {
        return Err("live source no longer matches the accepted installed candidate".into());
    }
    let activated = pending["activated_at_ms"]
        .as_u64()
        .ok_or("activation evidence has no write-admission time")?;
    let acceptance_sha = check_live(&config.live_acceptance, &manifest, activated, owner.0)?;
    let pg_config = PathBuf::from(
        pending["postgres_config"]
            .as_str()
            .ok_or("activation has no private PostgreSQL configuration")?,
    );
    if pending["postgres_config_sha256"].as_str() != Some(install::hash_file(&pg_config)?.as_str())
    {
        return Err("PostgreSQL authority configuration changed after activation".into());
    }
    let binary = manifest
        .binaries
        .iter()
        .find(|binary| binary.name == "devcoordinator2-migrate-authority")
        .ok_or("verified migration helper is unavailable")?;
    let attempt = format!("final-{}", random_hex(8)?);
    let archive = config.transaction_dir.join(format!("{attempt}.pgdump"));
    helper(
        runner,
        Path::new(&binary.path),
        "backup",
        &pg_config,
        &archive,
        &config
            .transaction_dir
            .join(format!("{attempt}-backup.json")),
        owner.0,
    )?;
    let verified = helper(
        runner,
        Path::new(&binary.path),
        "verify-recoverable-backup",
        &pg_config,
        &archive,
        &config
            .transaction_dir
            .join(format!("{attempt}-verified.json")),
        owner.0,
    )?;
    if verified["isolated_restore_removed"] != true {
        return Err("PostgreSQL backup verification has unfinished isolated recovery work".into());
    }
    // Recheck the live evidence after potentially long archive verification.
    if check_live(&config.live_acceptance, &manifest, activated, owner.0)? != acceptance_sha {
        return Err("live acceptance changed during finalization".into());
    }
    let targets = pending["retirement_targets"]
        .as_array()
        .ok_or("activation has no exact retirement target list")?
        .iter()
        .map(|target| {
            Ok((
                PathBuf::from(
                    target["path"]
                        .as_str()
                        .ok_or("retirement target path is missing")?,
                ),
                target["sha256"]
                    .as_str()
                    .ok_or("retirement target digest is missing")?
                    .to_owned(),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if targets.len() > 32 {
        return Err("activation retirement target list exceeds its bound".into());
    }
    let mut exact = std::collections::BTreeSet::new();
    if targets.iter().any(|(path, _)| !exact.insert(path.clone())) {
        return Err("activation retirement targets are duplicated".into());
    }
    let prior = if result_path.exists() {
        Some(private(&result_path, owner.0)?)
    } else {
        None
    };
    for (path, digest) in &targets {
        if !path.is_absolute() {
            return Err("legacy retirement target is not exact and absolute".into());
        }
        match path.symlink_metadata() {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.uid() != owner.0
                    || digest != &install::hash_file(path)?
                {
                    return Err("legacy authority identity changed; PostgreSQL remains active and sources stay protected".into());
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && prior.as_ref().is_some_and(|receipt| {
                        receipt["status"] == "retiring"
                            && receipt["source_commit"] == manifest.source_commit
                    }) => {}
            Err(_) => return Err("legacy authority retirement target is unavailable".into()),
        }
    }
    let mut receipt = FinalizationReceipt {
        schema: 1,
        status: "retiring".into(),
        source_commit: manifest.source_commit,
        live_acceptance_sha256: acceptance_sha,
        postgres_archive_sha256: install::hash_file(&archive)?,
        retired_sources: 0,
    };
    install::atomic_file(
        &result_path,
        &serde_json::to_vec_pretty(&receipt).map_err(|_| "cannot retain finalization evidence")?,
        0o600,
        Some(owner),
    )?;
    for (path, _) in targets {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(
                    "PostgreSQL remains active; legacy retirement is partial and may safely resume"
                        .into(),
                );
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| "legacy retirement directory could not be sealed")?;
        }
        receipt.retired_sources += 1;
    }
    receipt.status = "finalized".into();
    install::atomic_file(
        &result_path,
        &serde_json::to_vec_pretty(&receipt).map_err(|_| "cannot retain finalization result")?,
        0o600,
        Some(owner),
    )?;
    finish_protection_marker(&config.transaction_dir, &receipt, owner.0)?;
    Ok(receipt)
}
