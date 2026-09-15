//! Content-bound receipts for opt-in reuse of completed direct checks.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use devcoordinator2_executor_protocol::{ArtifactReceipt, CheckPlan, ExecutionPlan};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, mkdirat, open, openat, renameat, unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u8,
    key: String,
    origin_run_id: String,
    created_seconds: u64,
    artifacts: Vec<ArtifactReceipt>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn directory(root: &Path) -> Option<File> {
    let mut descriptor = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    for part in [".devcoordinator", "test", "cache"] {
        match mkdirat(&descriptor, part, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(_) => return None,
        }
        descriptor = openat(
            &descriptor,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
    }
    Some(File::from(descriptor))
}

fn binary(root: &Path, check: &CheckPlan) -> Option<PathBuf> {
    let command = check.command.as_ref()?.first()?;
    let program = Path::new(command);
    if program.is_absolute() {
        return program.canonicalize().ok();
    }
    if program.components().count() > 1 {
        return root.join(&check.cwd).join(program).canonicalize().ok();
    }
    let path = check
        .env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_else(|| "/usr/bin:/bin".into());
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|path| path.is_file())?
        .canonicalize()
        .ok()
}

pub fn key(
    root: &Path,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    inputs: &[ArtifactReceipt],
    consumed: &[ArtifactReceipt],
) -> Option<String> {
    if !check.cacheable || !check.retained_artifacts.is_empty() {
        return None;
    }
    let mut executable = File::open(binary(root, check)?).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = executable.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut declaration = check.clone();
    if plan.environment_files.contains_key(&check.name) {
        for name in [
            "PGHOST",
            "PGPORT",
            "PGUSER",
            "PGPASSWORD",
            "PGDATABASE",
            "DATABASE_URL",
        ] {
            declaration.env.remove(name);
        }
    }
    digest.update(
        serde_json::to_vec(&(
            1,
            std::env::consts::OS,
            std::env::consts::ARCH,
            &plan.source_digest,
            &plan.config_digest,
            &plan.test,
            &declaration,
            inputs,
            consumed,
        ))
        .ok()?,
    );
    Some(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

pub fn lookup(root: &Path, check: &CheckPlan, key: &str) -> Option<Vec<ArtifactReceipt>> {
    let directory = directory(root)?;
    let descriptor = openat(
        &directory,
        format!("{}.json", check.name),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = fstat(&descriptor).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_size > 65536
        || stat.st_mode & 0o077 != 0
    {
        return None;
    }
    let mut bytes = Vec::new();
    File::from(descriptor)
        .take(65537)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 65536 {
        return None;
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).ok()?;
    if receipt.schema != 1
        || receipt.key != key
        || receipt.created_seconds > now()
        || now().saturating_sub(receipt.created_seconds) > 86400
        || receipt
            .artifacts
            .iter()
            .map(|artifact| &artifact.path)
            .collect::<Vec<_>>()
            != check.produces.iter().collect::<Vec<_>>()
        || !crate::receipts_match(root, &receipt.artifacts).ok()?
    {
        return None;
    }
    Some(receipt.artifacts)
}

pub fn record(
    root: &Path,
    plan: &ExecutionPlan,
    check: &CheckPlan,
    key: &str,
    artifacts: &[ArtifactReceipt],
) {
    let Some(directory) = directory(root) else {
        return;
    };
    let receipt = Receipt {
        schema: 1,
        key: key.into(),
        origin_run_id: plan.run_id.clone(),
        created_seconds: now(),
        artifacts: artifacts.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&receipt) else {
        return;
    };
    let temporary = format!("{}-{}.tmp", check.name, plan.run_id);
    let Ok(descriptor) = openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) else {
        return;
    };
    let mut file = File::from(descriptor);
    if file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .is_ok()
    {
        let _ = renameat(
            &directory,
            temporary.as_str(),
            &directory,
            format!("{}.json", check.name),
        );
    }
    let _ = unlinkat(&directory, temporary.as_str(), AtFlags::empty());
}
