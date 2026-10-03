use super::{HostBackend, candidate, processes_reference};
use crate::storage::{
    blocked, fs, hash, json,
    model::{Context, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct MountEntry {
    pub source: PathBuf,
    pub target: PathBuf,
    pub entry: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Request {
    job_id: String,
    artifact_id: String,
    entry: MountEntry,
    device: u64,
    inode: u64,
    fstab_sha256: String,
    helper_sha256: String,
    source_commit: String,
}

fn fstab_path(config: &crate::config::Config) -> PathBuf {
    #[cfg(feature = "root-acceptance")]
    if config.unit_prefix.starts_with("devcoordinator2-rustint-") {
        return config.state_dir.join("storage-fixture.fstab");
    }
    let _ = config;
    PathBuf::from("/etc/fstab")
}

impl HostBackend {
    pub(super) fn save_mount_recovery(&self, r: &Record, job: &str) -> Result<(), ProtocolError> {
        let directory = self.config.state_dir.join("storage-recovery");
        std::fs::create_dir_all(&directory)
            .map_err(|_| unavailable("private_recovery_unavailable"))?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| unavailable("private_recovery_unavailable"))?;
        let path =
            request_path(&directory, job, &r.artifact.artifact_id)?.with_extension("fstab.before");
        if path.exists() {
            private_read(&path, 1024 * 1024, true)?;
        } else {
            write_private_new(
                &path,
                &private_read(&fstab_path(&self.config), 1024 * 1024, false)?,
            )?;
        }
        Ok(())
    }

    pub(super) fn validate_mount_recovery(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
        job: &str,
    ) -> Result<(), ProtocolError> {
        if r.artifact.protected || context.leased_artifacts.contains(&r.artifact.artifact_id) {
            return Err(blocked("protected_cleanup_recovery"));
        }
        let file = request_path(
            &self.config.state_dir.join("storage-mount-jobs"),
            job,
            &r.artifact.artifact_id,
        )?;
        let request: Request = serde_json::from_slice(&private_read(&file, 64 * 1024, true)?)
            .map_err(|_| blocked("mount_request_invalid"))?;
        let Locator::Mount {
            source,
            target,
            entry,
            device,
            inode,
        } = &r.locator
        else {
            return Err(blocked("mount_identity_invalid"));
        };
        if request.job_id != job
            || request.artifact_id != r.artifact.artifact_id
            || request.entry.source != *source
            || request.entry.target != *target
            || request.entry.entry != *entry
            || request.device != *device
            || request.inode != *inode
        {
            return Err(blocked("mount_recovery_changed"));
        }
        if fs::identity(source)? != (*device, *inode) {
            return Err(blocked("identity_changed"));
        }
        if context.current_paths.iter().any(|p| {
            source.starts_with(p)
                || p.starts_with(source)
                || target.starts_with(p)
                || p.starts_with(target)
        }) {
            return Err(blocked("current_deployment"));
        }
        self.validate_mount_consumers(r, context, selected)?;
        if processes_reference(&r.private_aliases)? {
            return Err(blocked("active_process"));
        }
        if fs::mount_targets()?
            .iter()
            .any(|p| p != target && p.starts_with(target))
        {
            return Err(blocked("unexpected_nested_mount"));
        }
        if self
            .mount_entries()?
            .iter()
            .any(|e| e.target == *target && e.entry != *entry)
        {
            return Err(blocked("mount_configuration_changed"));
        }
        Ok(())
    }

    pub(super) fn mount_entries(&self) -> Result<Vec<MountEntry>, ProtocolError> {
        let path = fstab_path(&self.config);
        #[cfg(feature = "root-acceptance")]
        if self
            .config
            .unit_prefix
            .starts_with("devcoordinator2-rustint-")
            && !path.exists()
        {
            return Ok(Vec::new());
        }
        read_entries(&path)
    }

    pub(super) fn mount_candidate(
        &self,
        e: &MountEntry,
        context: &Context,
    ) -> Result<Record, ProtocolError> {
        let (device, inode) = fs::identity(&e.source)?;
        let mut r = candidate(
            api::Kind::Mount,
            api::Effect::RuntimeResource,
            format!(
                "{} mount",
                e.source.file_name().unwrap_or_default().to_string_lossy()
            ),
            Locator::Mount {
                source: e.source.clone(),
                target: e.target.clone(),
                entry: e.entry.clone(),
                device,
                inode,
            },
            context,
        )?;
        r.resource_key = format!("mount:{}", hash(e.entry.as_bytes()));
        r.private_aliases = vec![e.source.clone(), e.target.clone()];
        r.last_activity_signature = hash(e.entry.as_bytes());
        r.artifact.filesystem_id = Some(format!("fs-{device}"));
        Ok(r)
    }

    pub(super) fn validate_mount(
        &self,
        r: &Record,
        context: &Context,
        _selected: &[String],
    ) -> Result<(), ProtocolError> {
        let Locator::Mount {
            source,
            target,
            entry,
            device,
            inode,
        } = &r.locator
        else {
            return Err(blocked("mount_identity_invalid"));
        };
        if fs::identity(source)? != (*device, *inode) {
            return Err(blocked("identity_changed"));
        }
        if fs::mount_targets()?.contains(target) && fs::identity(target)? != (*device, *inode) {
            return Err(blocked("mounted_identity_changed"));
        }
        if !self
            .mount_entries()?
            .iter()
            .any(|e| e.source == *source && e.target == *target && e.entry == *entry)
        {
            return Err(blocked("mount_configuration_changed"));
        }
        if context.current_paths.iter().any(|p| {
            source.starts_with(p)
                || p.starts_with(source)
                || target.starts_with(p)
                || p.starts_with(target)
        }) {
            return Err(blocked("current_deployment"));
        }
        if processes_reference(&[source.clone(), target.clone()])? {
            return Err(blocked("active_process"));
        }
        if fs::mount_targets()?
            .iter()
            .any(|p| p != target && p.starts_with(target))
        {
            return Err(blocked("unexpected_nested_mount"));
        }
        Ok(())
    }

    pub(super) fn remove_mount(&self, r: &Record, job_id: &str) -> Result<(), ProtocolError> {
        let Locator::Mount {
            source,
            target,
            entry,
            device,
            inode,
        } = &r.locator
        else {
            return Err(blocked("mount_identity_invalid"));
        };
        if unsafe { libc::geteuid() } != 0 {
            return Err(blocked("native_mount_authority_required"));
        }
        let executable =
            std::env::current_exe().map_err(|_| unavailable("helper_identity_unavailable"))?;
        let directory = self.config.state_dir.join("storage-mount-jobs");
        std::fs::create_dir_all(&directory)
            .map_err(|_| unavailable("private_mount_state_unavailable"))?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| unavailable("private_mount_state_unavailable"))?;
        let file = request_path(&directory, job_id, &r.artifact.artifact_id)?;
        let request = Request {
            job_id: job_id.into(),
            artifact_id: r.artifact.artifact_id.clone(),
            entry: MountEntry {
                source: source.clone(),
                target: target.clone(),
                entry: entry.clone(),
            },
            device: *device,
            inode: *inode,
            fstab_sha256: hash(&private_read(
                &fstab_path(&self.config),
                1024 * 1024,
                false,
            )?),
            helper_sha256: file_hash(&executable)?,
            source_commit: crate::SOURCE_COMMIT.into(),
        };
        if !file.exists() {
            write_private_new(&file, json(&request)?.as_bytes())?;
        }
        let args = vec![
            "--quiet".into(),
            "--wait".into(),
            "--pipe".into(),
            "--collect".into(),
            "--service-type=exec".into(),
            format!(
                "--unit={}-storage-{}-{}",
                self.config.unit_prefix, job_id, r.artifact.artifact_id
            )
            .into(),
            "--property=UMask=0077".into(),
            "--property=NoNewPrivileges=yes".into(),
            "--property=KillMode=control-group".into(),
            "--property=RuntimeMaxSec=180".into(),
            format!(
                "--setenv=DEVCOORDINATOR2_STATE_DIR={}",
                self.config.state_dir.display()
            )
            .into(),
            format!(
                "--setenv=DEVCOORDINATOR2_UNIT_PREFIX={}",
                self.config.unit_prefix
            )
            .into(),
            "--setenv=DEVCOORDINATOR2_INSTANCE_ENV=/nonexistent".into(),
            executable.into_os_string(),
            "storage-maintenance".into(),
            "--job-id".into(),
            job_id.into(),
            "--artifact-id".into(),
            r.artifact.artifact_id.clone().into(),
        ];
        let output = crate::metrics_source::run_bounded(
            Path::new("/usr/bin/systemd-run"),
            &args,
            Duration::from_secs(190),
        )
        .ok_or_else(|| unavailable("mount_helper_unavailable"))?;
        if !output.status.success() {
            for attempt in 0..100u32 {
                let evidence = file.with_extension(format!("attempt-{attempt}.output"));
                if !evidence.exists() {
                    write_private_new(&evidence, &output.stdout)?;
                    break;
                }
            }
            if let Some(message) = output
                .stdout
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                .find_map(|v| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_owned)
                })
                .filter(|v| {
                    v.len() <= 100 && v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
            {
                return Err(unavailable(&message));
            }
            return Err(unavailable("mount_retirement_incomplete"));
        }
        let receipt = file.with_extension("receipt.json");
        let data: serde_json::Value = serde_json::from_slice(&private_read(&receipt, 8192, true)?)
            .map_err(|_| unavailable("mount_receipt_invalid"))?;
        if data.get("job_id").and_then(|v| v.as_str()) != Some(job_id)
            || data.get("artifact_id").and_then(|v| v.as_str()) != Some(&r.artifact.artifact_id)
            || data.get("removed").and_then(|v| v.as_bool()) != Some(true)
        {
            return Err(unavailable("mount_receipt_invalid"));
        }
        Ok(())
    }
}

pub(crate) fn execute(
    config: &crate::config::Config,
    job_id: &str,
    artifact_id: &str,
) -> Result<(), ProtocolError> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(blocked("native_mount_authority_required"));
    }
    let path = request_path(
        &config.state_dir.join("storage-mount-jobs"),
        job_id,
        artifact_id,
    )?;
    let request: Request = serde_json::from_slice(&private_read(&path, 64 * 1024, true)?)
        .map_err(|_| blocked("mount_request_invalid"))?;
    if request.job_id != job_id
        || request.artifact_id != artifact_id
        || request.source_commit != crate::SOURCE_COMMIT
    {
        return Err(blocked("helper_identity_changed"));
    }
    if request.helper_sha256
        != file_hash(
            &std::env::current_exe().map_err(|_| unavailable("helper_identity_unavailable"))?,
        )?
    {
        return Err(blocked("helper_identity_changed"));
    }
    verify_installed_helper(config, &request)?;
    let connection = rusqlite::Connection::open_with_flags(
        config.state_dir.join("authority.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| unavailable("cleanup_job_unavailable"))?;
    let (state,plan): (String,String)=connection.query_row("SELECT j.state,p.plan_json FROM storage_jobs j JOIN storage_plans p ON json_extract(j.job_json,'$.plan_id')=p.plan_id WHERE j.job_id=?1",[job_id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|blocked("cleanup_job_unavailable"))?;
    if state != "running" {
        return Err(blocked("cleanup_job_not_running"));
    }
    let plan: crate::storage::model::StoredPlan = crate::storage::parse(&plan)?;
    if !plan.records.iter().any(|r|r.artifact.artifact_id==artifact_id&&matches!(&r.locator,Locator::Mount {source,target,entry,device,inode} if *source==request.entry.source&&*target==request.entry.target&&*entry==request.entry.entry&&*device==request.device&&*inode==request.inode)){return Err(blocked("mount_not_in_cleanup_plan"));}
    if fs::identity(&request.entry.source)? != (request.device, request.inode) {
        return Err(blocked("identity_changed"));
    }
    let fstab = fstab_path(config);
    retire_entry(&fstab, &request, &path)?;
    let mut retired_units = Vec::new();
    for suffix in ["automount", "mount"] {
        let args = vec![
            "--path".into(),
            format!("--suffix={suffix}").into(),
            request.entry.target.as_os_str().to_owned(),
        ];
        let output = crate::metrics_source::run_bounded(
            Path::new("/usr/bin/systemd-escape"),
            &args,
            Duration::from_secs(5),
        )
        .ok_or_else(|| unavailable("mount_unit_unavailable"))?;
        if !output.status.success() || output.truncated {
            return Err(unavailable("mount_unit_unavailable"));
        }
        let unit = String::from_utf8(output.stdout).map_err(|_| blocked("mount_unit_invalid"))?;
        let unit = unit.trim();
        if unit.is_empty() || unit.contains('/') || !unit.ends_with(&format!(".{suffix}")) {
            return Err(blocked("mount_unit_invalid"));
        }
        systemctl(&["stop", unit])?;
        retired_units.push(unit.to_owned());
    }
    systemctl(&["daemon-reload"])?;
    for unit in retired_units {
        let args = vec![
            "show".into(),
            "--property=LoadState".into(),
            "--value".into(),
            unit.into(),
        ];
        let state = crate::metrics_source::run_bounded(
            Path::new("/usr/bin/systemctl"),
            &args,
            Duration::from_secs(10),
        )
        .ok_or_else(|| unavailable("mount_unit_unavailable"))?;
        if !state.status.success() || state.truncated || state.stdout != b"not-found\n" {
            return Err(blocked("mount_can_be_reactivated"));
        }
    }
    // Trigger normal access after removing the automount definition, then prove
    // that access did not recreate a mount before permitting volume deletion.
    drop(fs::open_directory(&request.entry.target)?);
    if fs::mount_targets()?
        .iter()
        .any(|p| p == &request.entry.target || p.starts_with(&request.entry.target))
    {
        return Err(blocked("mount_still_active"));
    }
    if read_entries(&fstab)?
        .iter()
        .any(|e| e.target == request.entry.target)
    {
        return Err(blocked("mount_configuration_changed"));
    }
    let receipt = serde_json::json!({"job_id":job_id,"artifact_id":artifact_id,"removed":true,"source_commit":crate::SOURCE_COMMIT});
    let target = path.with_extension("receipt.json");
    if !target.exists() {
        write_private_new(&target, json(&receipt)?.as_bytes())?;
    }
    Ok(())
}

fn retire_entry(fstab: &Path, request: &Request, request_path: &Path) -> Result<(), ProtocolError> {
    let before = private_read(fstab, 1024 * 1024, false)?;
    let text = std::str::from_utf8(&before).map_err(|_| blocked("fstab_encoding_invalid"))?;
    let matching = text.lines().filter(|l| *l == request.entry.entry).count();
    let backup = request_path.with_extension("fstab.before");
    if matching == 0 && backup.exists() {
        let original = private_read(&backup, 1024 * 1024, true)?;
        if hash(&original) != request.fstab_sha256 {
            return Err(blocked("mount_recovery_changed"));
        }
        let expected = std::str::from_utf8(&original)
            .map_err(|_| blocked("fstab_encoding_invalid"))?
            .split_inclusive('\n')
            .filter(|line| {
                line.trim_end_matches('\n').trim_end_matches('\r') != request.entry.entry
            })
            .collect::<String>();
        if hash(expected.as_bytes()) != hash(&before) {
            return Err(blocked("mount_configuration_changed"));
        }
        if read_entries(fstab)?
            .iter()
            .any(|entry| entry.target == request.entry.target)
        {
            return Err(blocked("mount_configuration_changed"));
        }
        return Ok(());
    }
    if matching != 1 || hash(&before) != request.fstab_sha256 {
        return Err(blocked("mount_configuration_changed"));
    }
    if !backup.exists() {
        write_private_new(&backup, &before)?;
    }
    let after = text
        .split_inclusive('\n')
        .filter(|line| line.trim_end_matches('\n').trim_end_matches('\r') != request.entry.entry)
        .collect::<String>();
    let parent = fstab
        .parent()
        .ok_or_else(|| blocked("fstab_identity_invalid"))?;
    let before_meta =
        std::fs::symlink_metadata(fstab).map_err(|_| unavailable("fstab_unavailable"))?;
    let temp = parent.join(format!(
        ".devcoordinator2-fstab-{}-{}",
        request.job_id, request.artifact_id
    ));
    if temp.exists() {
        if private_read(&temp, 1024 * 1024, false)? != after.as_bytes() {
            return Err(blocked("mount_recovery_changed"));
        }
    } else {
        write_private_new(&temp, after.as_bytes())?;
    }
    std::fs::set_permissions(
        &temp,
        std::fs::Permissions::from_mode(before_meta.mode() & 0o777),
    )
    .map_err(|_| unavailable("fstab_mode_failed"))?;
    let fresh = std::fs::symlink_metadata(fstab).map_err(|_| unavailable("fstab_unavailable"))?;
    if (fresh.dev(), fresh.ino()) != (before_meta.dev(), before_meta.ino())
        || hash(&private_read(fstab, 1024 * 1024, false)?) != request.fstab_sha256
    {
        let _ = std::fs::remove_file(temp);
        return Err(blocked("mount_configuration_changed"));
    }
    std::fs::rename(&temp, fstab).map_err(|_| unavailable("fstab_replace_failed"))?;
    fs::open_directory(parent)?
        .sync_all()
        .map_err(|_| unavailable("fstab_sync_failed"))?;
    Ok(())
}

fn read_entries(path: &Path) -> Result<Vec<MountEntry>, ProtocolError> {
    let bytes = private_read(path, 1024 * 1024, false)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| blocked("fstab_encoding_invalid"))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() < 4 || parts[0].starts_with('#') || !parts[3].split(',').any(|v| v == "bind")
        {
            continue;
        }
        let source = PathBuf::from(fs::unescape_mount(parts[0]));
        let target = PathBuf::from(fs::unescape_mount(parts[1]));
        if source.is_absolute() && target.is_absolute() {
            entries.push(MountEntry {
                source,
                target,
                entry: line.into(),
            });
        }
    }
    Ok(entries)
}

pub(super) fn request_path(
    dir: &Path,
    job: &str,
    artifact: &str,
) -> Result<PathBuf, ProtocolError> {
    if [job, artifact]
        .iter()
        .any(|s| s.is_empty() || s.len() > 64 || !s.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Err(blocked("mount_request_identity_invalid"));
    }
    Ok(dir.join(format!("{job}-{artifact}.json")))
}

fn verify_installed_helper(
    config: &crate::config::Config,
    request: &Request,
) -> Result<(), ProtocolError> {
    let manifest = PathBuf::from("/etc/devcoordinator2/install-manifest.json");
    #[cfg(feature = "root-acceptance")]
    let manifest = if config.unit_prefix.starts_with("devcoordinator2-rustint-") {
        config.state_dir.join("storage-fixture-install.json")
    } else {
        manifest
    };
    let _ = config;
    let value: serde_json::Value =
        serde_json::from_slice(&private_read(&manifest, 1024 * 1024, true)?)
            .map_err(|_| blocked("installed_helper_unverified"))?;
    if value.get("source_commit").and_then(|v| v.as_str()) != Some(crate::SOURCE_COMMIT) {
        return Err(blocked("installed_helper_unverified"));
    }
    let executable =
        std::env::current_exe().map_err(|_| unavailable("helper_identity_unavailable"))?;
    let verified = value
        .get("binaries")
        .and_then(|v| v.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry.get("name").and_then(|v| v.as_str()) == Some("devcoordinator2")
                    && entry
                        .get("path")
                        .and_then(|v| v.as_str())
                        .is_some_and(|p| Path::new(p) == executable)
                    && entry.get("sha256").and_then(|v| v.as_str()) == Some(&request.helper_sha256)
                    && entry.get("source_commit").and_then(|v| v.as_str())
                        == Some(crate::SOURCE_COMMIT)
            })
        });
    if !verified {
        return Err(blocked("installed_helper_unverified"));
    }
    Ok(())
}
pub(super) fn private_read(path: &Path, max: u64, private: bool) -> Result<Vec<u8>, ProtocolError> {
    let parent = fs::open_directory(
        path.parent()
            .ok_or_else(|| blocked("private_path_invalid"))?,
    )?;
    let fd = rustix::fs::openat(
        &parent,
        path.file_name()
            .ok_or_else(|| blocked("private_path_invalid"))?,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| unavailable("private_record_unavailable"))?;
    let mut file = File::from(fd);
    let m = file
        .metadata()
        .map_err(|_| unavailable("private_metadata_unavailable"))?;
    if !m.is_file() || m.len() > max || (private && (m.uid() != 0 || m.mode() & 0o077 != 0)) {
        return Err(blocked("private_record_invalid"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable("private_read_failed"))?;
    if bytes.len() as u64 > max {
        return Err(blocked("private_record_too_large"));
    }
    Ok(bytes)
}
pub(super) fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), ProtocolError> {
    let parent = fs::open_directory(
        path.parent()
            .ok_or_else(|| blocked("private_path_invalid"))?,
    )?;
    let fd = rustix::fs::openat(
        &parent,
        path.file_name()
            .ok_or_else(|| blocked("private_path_invalid"))?,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(|_| unavailable("private_write_failed"))?;
    let mut file = File::from(fd);
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .and_then(|_| parent.sync_all())
        .map_err(|_| unavailable("private_write_failed"))
}
fn file_hash(path: &Path) -> Result<String, ProtocolError> {
    use sha2::Digest;
    let mut file = File::open(path).map_err(|_| unavailable("helper_identity_unavailable"))?;
    let mut h = sha2::Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| unavailable("helper_identity_unavailable"))?;
        if n == 0 {
            break;
        }
        h.update(&buffer[..n]);
    }
    Ok(crate::storage::hex(&h.finalize()))
}
fn systemctl(args: &[&str]) -> Result<(), ProtocolError> {
    let args = args
        .iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    let output = crate::metrics_source::run_bounded(
        Path::new("/usr/bin/systemctl"),
        &args,
        Duration::from_secs(60),
    )
    .ok_or_else(|| unavailable("mount_manager_unavailable"))?;
    if !output.status.success() {
        if args.first().is_some_and(|s| s == "stop") && args.len() == 2 {
            let inspect = vec![
                "show".into(),
                "--property=LoadState".into(),
                "--value".into(),
                args[1].clone(),
            ];
            if let Some(state) = crate::metrics_source::run_bounded(
                Path::new("/usr/bin/systemctl"),
                &inspect,
                Duration::from_secs(10),
            ) && state.status.success()
                && !state.truncated
                && state.stdout == b"not-found\n"
            {
                return Ok(());
            }
        }
        return Err(unavailable("mount_manager_action_failed"));
    }
    Ok(())
}
