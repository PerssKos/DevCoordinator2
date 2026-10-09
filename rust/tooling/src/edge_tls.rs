//! Source-owned adoption of one configured Certbot lineage by the stable edge.
//! Private snapshots and the existing deploy event are the only recovery/event
//! mechanisms; this module never changes routes, grants, or application units.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::install::{self, CommandOutput, CommandRequest, CommandRunner};

const UNIT: &str = "devcoordinator2-edge.service";
const FILE_CAP: u64 = 256 * 1024;
const HOOK: &str = include_str!("../../../deploy/devcoordinator2-certbot-deploy");
const INSPECT: &str = include_str!("../../../deploy/edge-tls-inspect.cjs");
const PROBE: &str = include_str!("../../../deploy/edge-tls-probe.cjs");

#[derive(Clone)]
struct Layout {
    configuration: PathBuf,
    manifest: PathBuf,
    edge_env: PathBuf,
    edge_unit: PathBuf,
    certificate: PathBuf,
    key: PathBuf,
    hook: PathBuf,
    recovery: PathBuf,
    owner: (u32, u32),
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            configuration: "/etc/devcoordinator2/edge/tls-renewal.json".into(),
            manifest: "/etc/devcoordinator2/install-manifest.json".into(),
            edge_env: "/etc/devcoordinator2/edge.env".into(),
            edge_unit: "/etc/systemd/system/devcoordinator2-edge.service".into(),
            certificate: "/etc/devcoordinator2/edge/tls.crt".into(),
            key: "/etc/devcoordinator2/edge/tls.key".into(),
            hook: "/etc/letsencrypt/renewal-hooks/deploy/devcoordinator2-edge".into(),
            recovery: "/var/lib/devcoordinator2/cutover/tls-renewal".into(),
            owner: (0, 0),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    schema: u32,
    lineage: PathBuf,
}

#[derive(Serialize)]
pub struct RenewalReceipt {
    pub status: &'static str,
    pub recovery_retained: bool,
}

#[derive(Deserialize)]
struct Certificate {
    fingerprint: String,
    expires_at_ms: i64,
    currently_valid: bool,
}

struct SavedFile {
    bytes: Vec<u8>,
    mode: u32,
    owner: (u32, u32),
}

/// Register only the stable edge's exact private lineage and existing deploy hook.
/// Call after the reviewed release activation has installed this tooling binary.
pub fn configure<R: CommandRunner>(
    lineage: &Path,
    source_commit: &str,
    runner: &R,
) -> Result<RenewalReceipt, String> {
    require_root()?;
    configure_at(&Layout::default(), lineage, source_commit, runner)
}

/// Certbot's successful-renewal event supplies the lineage; unrelated events skip.
pub fn deploy<R: CommandRunner>(
    lineage: &Path,
    source_commit: &str,
    runner: &R,
) -> Result<RenewalReceipt, String> {
    require_root()?;
    deploy_at(&Layout::default(), lineage, source_commit, runner)
}

fn require_root() -> Result<(), String> {
    if rustix::process::geteuid().as_raw() != 0 {
        return Err("TLS renewal maintenance must run as root".into());
    }
    Ok(())
}

fn configure_at<R: CommandRunner>(
    layout: &Layout,
    lineage: &Path,
    source_commit: &str,
    runner: &R,
) -> Result<RenewalReceipt, String> {
    let _lock = lock(layout)?;
    verify_installation(layout, source_commit, runner)?;
    let lineage = real_directory(lineage)?;
    let settings = edge_settings(layout)?;
    let candidate = snapshot_candidate(layout, &lineage)?;
    let staged = private_transaction(layout)?;
    save(&staged.join("candidate.crt"), &candidate.0, layout.owner)?;
    save(&staged.join("candidate.key"), &candidate.1, layout.owner)?;
    inspect(&staged, "candidate", &settings.hosts, false, runner)?;
    if let Ok(existing) = read_file(&layout.hook, layout.owner.0, false)
        && existing.bytes != HOOK.as_bytes()
    {
        return Err("the named Certbot hook belongs to another workflow".into());
    }
    // Existing malformed/nonregular targets are refusals, not absent files.
    if layout.hook.symlink_metadata().is_ok() {
        read_file(&layout.hook, layout.owner.0, false)?;
    }
    if layout.configuration.symlink_metadata().is_ok() {
        read_configuration(layout)?;
    }
    ensure_real_parent(
        layout
            .configuration
            .parent()
            .ok_or("missing configuration parent")?,
    )?;
    let config = serde_json::to_vec(&Configuration { schema: 1, lineage })
        .map_err(|_| "cannot encode TLS renewal configuration")?;
    install::atomic_file(&layout.configuration, &config, 0o600, Some(layout.owner))?;
    if let Some(parent) = layout.hook.parent() {
        ensure_real_parent(parent)?;
    }
    install::atomic_file(&layout.hook, HOOK.as_bytes(), 0o700, Some(layout.owner))?;
    Ok(RenewalReceipt {
        status: "configured",
        recovery_retained: true,
    })
}

fn deploy_at<R: CommandRunner>(
    layout: &Layout,
    lineage: &Path,
    source_commit: &str,
    runner: &R,
) -> Result<RenewalReceipt, String> {
    let _lock = lock(layout)?;
    let configuration = read_configuration(layout)?;
    let event_lineage = real_directory(lineage)?;
    if event_lineage != configuration.lineage {
        return Ok(RenewalReceipt {
            status: "ignored",
            recovery_retained: false,
        });
    }
    verify_installation(layout, source_commit, runner)?;
    service_identity(layout, runner)?;
    let settings = edge_settings(layout)?;
    let old_certificate = read_file(&layout.certificate, layout.owner.0, true)?;
    let old_key = read_file(&layout.key, layout.owner.0, true)?;
    let (certificate, key) = snapshot_candidate(layout, &event_lineage)?;
    let transaction = private_transaction(layout)?;
    save(
        &transaction.join("previous.crt"),
        &old_certificate.bytes,
        layout.owner,
    )?;
    save(
        &transaction.join("previous.key"),
        &old_key.bytes,
        layout.owner,
    )?;
    save(
        &transaction.join("candidate.crt"),
        &certificate,
        layout.owner,
    )?;
    save(&transaction.join("candidate.key"), &key, layout.owner)?;
    let candidate = inspect(&transaction, "candidate", &settings.hosts, false, runner)?;
    let previous = inspect(&transaction, "previous", &[], true, runner)?;
    if old_certificate.bytes == certificate && old_key.bytes == key {
        record(&transaction, "unchanged", layout.owner)?;
        return Ok(RenewalReceipt {
            status: "unchanged",
            recovery_retained: true,
        });
    }
    if candidate.expires_at_ms <= previous.expires_at_ms {
        return Err("renewed certificate must extend the existing expiry".into());
    }
    // Recheck after inspection: no stale source or unexpected service is restarted.
    verify_installation(layout, source_commit, runner)?;
    service_identity(layout, runner)?;
    record(&transaction, "activating", layout.owner)?;
    let activation = (|| {
        replace(&layout.certificate, &certificate, &old_certificate)?;
        replace(&layout.key, &key, &old_key)?;
        systemctl(runner, &["restart", UNIT])?;
        service_identity(layout, runner)?;
        probe(&settings, &candidate, runner)?;
        record(&transaction, "renewed", layout.owner)
    })();
    if activation.is_err() {
        // Attempt both file restorations even when one fails; retain all recovery
        // bytes and the terminal status rather than leaking subprocess output.
        let cert_restored = replace(
            &layout.certificate,
            &old_certificate.bytes,
            &old_certificate,
        );
        let key_restored = replace(&layout.key, &old_key.bytes, &old_key);
        let recovery = if cert_restored.is_ok() && key_restored.is_ok() {
            systemctl(runner, &["restart", UNIT])
                .and_then(|_| service_identity(layout, runner))
                .and_then(|_| {
                    if previous.currently_valid {
                        probe(&settings, &previous, runner)
                    } else {
                        Ok(())
                    }
                })
        } else {
            Err("previous TLS files could not be restored".into())
        };
        let recovered = recovery.is_ok();
        let _ = record(
            &transaction,
            if recovered {
                "rolled_back"
            } else {
                "rollback_failed"
            },
            layout.owner,
        );
        return Err(if recovered {
            "renewed TLS activation failed; the previous edge was restored and private recovery retained"
        } else {
            "renewed TLS activation and recovery failed; private recovery is retained for owner repair"
        }.into());
    }
    Ok(RenewalReceipt {
        status: "renewed",
        recovery_retained: true,
    })
}

fn verify_installation<R: CommandRunner>(
    layout: &Layout,
    source_commit: &str,
    runner: &R,
) -> Result<(), String> {
    let manifest =
        install::read_and_verify_manifest_owned(&layout.manifest, runner, layout.owner.0)
            .map_err(|_| "installed release verification failed")?;
    let root = Path::new(&manifest.source_root);
    let checkout = install::validate_live_checkout(root, true, runner)
        .map_err(|_| "canonical source is not clean current main")?;
    if checkout != manifest.source_commit || checkout != source_commit {
        return Err("TLS maintenance requires the currently installed canonical release".into());
    }
    let expected = install::render_edge_unit(root, false)?;
    let unit = read_file(&layout.edge_unit, layout.owner.0, false)?;
    if unit.bytes != expected.as_bytes() {
        return Err("stable edge unit differs from the reviewed canonical source".into());
    }
    Ok(())
}

fn service_identity<R: CommandRunner>(layout: &Layout, runner: &R) -> Result<(), String> {
    let result = systemctl(
        runner,
        &["show", UNIT, "--property=FragmentPath", "--value"],
    )?;
    if result.stdout.trim() != layout.edge_unit.to_str().ok_or("invalid edge unit path")? {
        return Err("stable edge service has an unexpected loaded identity".into());
    }
    systemctl(runner, &["is-active", "--quiet", UNIT])?;
    Ok(())
}

struct EdgeSettings {
    console_host: String,
    port: u16,
    hosts: Vec<String>,
}

fn edge_settings(layout: &Layout) -> Result<EdgeSettings, String> {
    let env = read_file(&layout.edge_env, layout.owner.0, false)?;
    let text = std::str::from_utf8(&env.bytes).map_err(|_| "invalid edge environment")?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            values.insert(
                name.trim(),
                value.trim().trim_matches('"').trim_matches('\''),
            );
        }
    }
    if values.get("EDGE_HTTP_ONLY") == Some(&"1") {
        return Err("TLS renewal is unavailable for an HTTP-only canary".into());
    }
    let base = values
        .get("EDGE_BASE_DOMAIN")
        .ok_or("edge base domain is not configured")?
        .trim_end_matches('.')
        .to_lowercase();
    let console = values
        .get("EDGE_CONSOLE_HOST")
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("console.{base}"))
        .to_lowercase();
    if !valid_hostname(&base) || !valid_hostname(&console) {
        return Err("edge hostnames are invalid".into());
    }
    let port = values
        .get("EDGE_HTTPS_PORT")
        .unwrap_or(&"443")
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or("edge HTTPS port is invalid")?;
    let hosts = vec![base.clone(), format!("*.{base}"), console.clone()];
    Ok(EdgeSettings {
        console_host: console,
        port,
        hosts,
    })
}

fn valid_hostname(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn read_configuration(layout: &Layout) -> Result<Configuration, String> {
    let raw = read_file(&layout.configuration, layout.owner.0, true)?;
    let configuration: Configuration =
        serde_json::from_slice(&raw.bytes).map_err(|_| "invalid TLS renewal configuration")?;
    if configuration.schema != 1 || real_directory(&configuration.lineage)? != configuration.lineage
    {
        return Err("TLS renewal lineage identity is invalid".into());
    }
    Ok(configuration)
}

fn snapshot_candidate(layout: &Layout, lineage: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    // Certbot's live entries are intentional symlinks to its root-owned archive.
    // Resolve once, then open nofollow and retain immutable private byte snapshots.
    let cert = lineage
        .join("fullchain.pem")
        .canonicalize()
        .map_err(|_| "renewed certificate is unavailable")?;
    let key = lineage
        .join("privkey.pem")
        .canonicalize()
        .map_err(|_| "renewed private key is unavailable")?;
    Ok((
        read_file(&cert, layout.owner.0, false)?.bytes,
        read_file(&key, layout.owner.0, true)?.bytes,
    ))
}

fn read_file(path: &Path, owner: u32, private: bool) -> Result<SavedFile, String> {
    ensure_real_parent(path.parent().ok_or("missing TLS file parent")?)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "TLS maintenance file is unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "TLS maintenance file cannot be inspected")?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.len() > FILE_CAP
        || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        return Err("TLS maintenance file has an invalid owner, mode, type, or size".into());
    }
    let mut bytes = Vec::new();
    file.take(FILE_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "TLS maintenance file cannot be read")?;
    if bytes.len() as u64 > FILE_CAP {
        return Err("TLS maintenance file is too large".into());
    }
    Ok(SavedFile {
        bytes,
        mode: metadata.mode() & 0o777,
        owner: (metadata.uid(), metadata.gid()),
    })
}

fn ensure_real_parent(path: &Path) -> Result<(), String> {
    if std::path::absolute(path).map_err(|_| "invalid TLS directory")?
        != path
            .canonicalize()
            .map_err(|_| "TLS directory is unavailable")?
    {
        return Err("TLS maintenance directories may not contain symlinks".into());
    }
    Ok(())
}

fn real_directory(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path).map_err(|_| "invalid renewal lineage")?;
    let resolved = absolute
        .canonicalize()
        .map_err(|_| "renewal lineage is unavailable")?;
    if !resolved.is_dir() {
        return Err("renewal lineage must be a directory".into());
    }
    Ok(resolved)
}

fn private_directory(path: &Path, owner: (u32, u32)) -> Result<(), String> {
    if !path.exists() {
        ensure_real_parent(path.parent().ok_or("missing private directory parent")?)?;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| "cannot create private TLS directory")?;
    }
    ensure_real_parent(path)?;
    let metadata = path
        .symlink_metadata()
        .map_err(|_| "cannot inspect private TLS directory")?;
    if !metadata.is_dir() || metadata.uid() != owner.0 || metadata.mode() & 0o077 != 0 {
        return Err("TLS recovery directory must be private and owner-controlled".into());
    }
    Ok(())
}

fn lock(layout: &Layout) -> Result<File, String> {
    private_directory(&layout.recovery, layout.owner)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(layout.recovery.join("maintenance.lock"))
        .map_err(|_| "cannot open TLS maintenance lock")?;
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect TLS maintenance lock")?;
    if !metadata.is_file() || metadata.uid() != layout.owner.0 || metadata.mode() & 0o077 != 0 {
        return Err("TLS maintenance lock must be private and owner-controlled".into());
    }
    // SAFETY: the descriptor is valid throughout the call and owned by `file`.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another TLS renewal maintenance operation is active".into());
    }
    Ok(file)
}

fn private_transaction(layout: &Layout) -> Result<PathBuf, String> {
    let mut random = [0u8; 12];
    getrandom::fill(&mut random).map_err(|_| "cannot create TLS recovery identity")?;
    let transaction = layout.recovery.join(format!("renewal-{}", hex(&random)));
    private_directory(&transaction, layout.owner)?;
    Ok(transaction)
}

fn save(path: &Path, bytes: &[u8], owner: (u32, u32)) -> Result<(), String> {
    install::atomic_file(path, bytes, 0o600, Some(owner))
}

fn replace(path: &Path, bytes: &[u8], previous: &SavedFile) -> Result<(), String> {
    install::atomic_file(path, bytes, previous.mode, Some(previous.owner))
}

fn record(transaction: &Path, status: &str, owner: (u32, u32)) -> Result<(), String> {
    let raw = serde_json::to_vec(&serde_json::json!({"schema":1,"status":status}))
        .map_err(|_| "cannot encode TLS recovery status")?;
    save(&transaction.join("receipt.json"), &raw, owner)
}

fn inspect<R: CommandRunner>(
    transaction: &Path,
    prefix: &str,
    hosts: &[String],
    current: bool,
    runner: &R,
) -> Result<Certificate, String> {
    let hosts = serde_json::to_string(hosts).map_err(|_| "invalid TLS coverage request")?;
    let output = run(
        runner,
        "/usr/bin/node",
        vec![
            "-e".into(),
            INSPECT.into(),
            transaction.join(format!("{prefix}.crt")).into_os_string(),
            transaction.join(format!("{prefix}.key")).into_os_string(),
            hosts.into(),
            if current { "current" } else { "renewed" }.into(),
        ],
    )
    .map_err(|_| "TLS certificate pair, validity, or hostname coverage is invalid")?;
    let certificate: Certificate =
        serde_json::from_str(&output.stdout).map_err(|_| "invalid TLS inspection result")?;
    if certificate.fingerprint.len() != 64
        || !certificate
            .fingerprint
            .bytes()
            .all(|c| c.is_ascii_hexdigit())
    {
        return Err("invalid TLS inspection fingerprint".into());
    }
    Ok(certificate)
}

fn probe<R: CommandRunner>(
    settings: &EdgeSettings,
    certificate: &Certificate,
    runner: &R,
) -> Result<(), String> {
    let output = run(
        runner,
        "/usr/bin/node",
        vec![
            "-e".into(),
            PROBE.into(),
            settings.console_host.clone().into(),
            settings.port.to_string().into(),
            certificate.fingerprint.clone().into(),
        ],
    )?;
    if output.stdout != "{\"verified\":true}" {
        return Err("stable edge TLS verification failed".into());
    }
    Ok(())
}

fn systemctl<R: CommandRunner>(runner: &R, args: &[&str]) -> Result<CommandOutput, String> {
    run(
        runner,
        "/usr/bin/systemctl",
        args.iter().map(OsString::from).collect(),
    )
}

fn run<R: CommandRunner>(
    runner: &R,
    program: &str,
    args: Vec<OsString>,
) -> Result<CommandOutput, String> {
    let output = runner
        .run(&CommandRequest {
            program: program.into(),
            args,
            environment: BTreeMap::new(),
            clear_environment: true,
        })
        .map_err(|_| "TLS maintenance command could not run")?;
    if !output.success || output.stdout_truncated || output.stderr_truncated {
        return Err("TLS maintenance command failed".into());
    }
    Ok(output)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "edge_tls_tests.rs"]
mod tests;
