use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use devcoordinator2_executor_protocol::{
    ArtifactReceipt, MAX_REPORT_BYTES, MAX_RETAINED_ARTIFACT_FILES,
    MAX_RETAINED_ARTIFACT_MANIFEST_BYTES, ProofKind, RetainedArtifactReceipt, RetainedArtifactSpec,
    ValidationTier,
};
use rustix::fs::{self as unix_fs, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ExecutorError;

const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const COPY_BLOCK_BYTES: usize = 1024 * 1024;
pub const RETAINED_ARTIFACT_MANIFEST: &str = "retained-artifacts.json";
pub const RETAINED_ARTIFACT_DIRECTORY: &str = "retained";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactTree {
    pub name: String,
    pub size: u64,
    pub files: u32,
    pub sha256: String,
    pub entries: Vec<RetainedArtifactFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactManifest {
    pub schema: u8,
    pub kind: String,
    pub run_id: String,
    pub test: String,
    pub check: String,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub source_sha256: String,
    pub config_sha256: String,
    pub artifacts: Vec<RetainedArtifactTree>,
}

#[derive(Clone, Copy, Debug)]
pub struct RetainedArtifactIdentity<'a> {
    pub run_id: &'a str,
    pub test: &'a str,
    pub check: &'a str,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub source_sha256: &'a str,
    pub config_sha256: &'a str,
}

pub fn source_digest(worktree_root: &Path) -> Result<String, ExecutorError> {
    let root = worktree_root
        .canonicalize()
        .map_err(|error| ExecutorError::new(format!("cannot resolve worktree root: {error}")))?;
    if !root.is_dir() {
        return Err(ExecutorError::new("worktree root is not a directory"));
    }
    let index = git_output(&root, &["ls-files", "--stage", "-z"])?;
    let modified = decode_paths(git_output(
        &root,
        &["diff-files", "--name-only", "--no-ext-diff", "-z"],
    )?)?;
    let untracked = decode_paths(git_output(
        &root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?)?;

    let mut digest = Sha256::new();
    digest.update(b"devcoordinator2-source-v2\0index\0");
    digest.update(&index);
    digest.update(b"\0");
    let paths: BTreeSet<String> = modified.into_iter().chain(untracked).collect();
    for relative in paths {
        if relative == ".devcoordinator" || relative.starts_with(".devcoordinator/") {
            continue;
        }
        // diff-files reports stat-cache candidates, not necessarily changed
        // bytes. Never make the identity depend on whether git status happened
        // to refresh that cache. Compare raw blobs without clean/text filters.
        if matches_index_blob(&root, &relative, &index)? {
            continue;
        }
        hash_source_path(&root, &relative, &mut digest)?;
    }
    Ok(lower_hex(&digest.finalize()))
}

pub fn artifact_receipts(
    worktree_root: &Path,
    paths: &[String],
) -> Result<Vec<ArtifactReceipt>, ExecutorError> {
    let root = worktree_root
        .canonicalize()
        .map_err(|error| ExecutorError::new(format!("cannot resolve worktree root: {error}")))?;
    paths
        .iter()
        .map(|relative| artifact_receipt(&root, relative))
        .collect()
}

pub fn receipts_match(
    worktree_root: &Path,
    receipts: &[ArtifactReceipt],
) -> Result<bool, ExecutorError> {
    let paths: Vec<String> = receipts.iter().map(|row| row.path.clone()).collect();
    Ok(artifact_receipts(worktree_root, &paths)? == receipts)
}

/// Snapshot declared evidence directories into one settled check's retained leaf.
///
/// Source traversal and file opens are descriptor-relative and never follow links. The
/// destination is a new private directory beneath the executor-owned evidence leaf.
pub fn retain_artifact_trees(
    worktree_root: &Path,
    evidence_dir: &Path,
    identity: &RetainedArtifactIdentity<'_>,
    specs: &[RetainedArtifactSpec],
) -> Result<Vec<RetainedArtifactReceipt>, ExecutorError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let root = open_directory_path(worktree_root, "worktree root")?;
    fs::create_dir_all(evidence_dir).map_err(|error| {
        ExecutorError::new(format!(
            "cannot prepare retained evidence directory: {error}"
        ))
    })?;
    fs::set_permissions(evidence_dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
        ExecutorError::new(format!(
            "cannot protect retained evidence directory: {error}"
        ))
    })?;
    let retained = evidence_dir.join(RETAINED_ARTIFACT_DIRECTORY);
    fs::create_dir(&retained).map_err(|error| {
        ExecutorError::new(format!("cannot create retained artifact store: {error}"))
    })?;
    fs::set_permissions(&retained, fs::Permissions::from_mode(0o700)).map_err(|error| {
        ExecutorError::new(format!("cannot protect retained artifact store: {error}"))
    })?;

    let result = (|| {
        let mut trees = Vec::new();
        for spec in specs {
            validate_relative(&spec.path)?;
            let source = open_relative_directory(&root, &spec.path)?;
            let destination = retained.join(&spec.name);
            fs::create_dir(&destination).map_err(|error| {
                ExecutorError::new(format!(
                    "cannot create retained artifact {:?}: {error}",
                    spec.name
                ))
            })?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    ExecutorError::new(format!(
                        "cannot protect retained artifact {:?}: {error}",
                        spec.name
                    ))
                },
            )?;
            let mut entries = Vec::new();
            let mut size = 0_u64;
            copy_tree(
                &source,
                &destination,
                "",
                spec.max_bytes,
                &mut size,
                &mut entries,
            )?;
            if entries.is_empty() {
                return Err(ExecutorError::new(format!(
                    "declared retained artifact {:?} is empty",
                    spec.path
                )));
            }
            entries.sort_by(|left, right| left.path.cmp(&right.path));
            let digest = tree_digest(&entries);
            trees.push(RetainedArtifactTree {
                name: spec.name.clone(),
                size,
                files: u32::try_from(entries.len())
                    .map_err(|_| ExecutorError::new("retained artifact file count overflow"))?,
                sha256: digest,
                entries,
            });
        }
        let manifest = RetainedArtifactManifest {
            schema: 1,
            kind: "devcoordinator2-retained-artifact-trees".into(),
            run_id: identity.run_id.into(),
            test: identity.test.into(),
            check: identity.check.into(),
            requested_tier: identity.requested_tier,
            readiness_eligible: identity.readiness_eligible,
            proof: identity.proof,
            source_sha256: identity.source_sha256.into(),
            config_sha256: identity.config_sha256.into(),
            artifacts: trees,
        };
        let retained_directory = File::open(&retained).map_err(|error| {
            ExecutorError::new(format!("cannot open retained artifact store: {error}"))
        })?;
        retained_directory.sync_all().map_err(|error| {
            ExecutorError::new(format!("cannot sync retained artifact store: {error}"))
        })?;
        let mut manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| {
            ExecutorError::new(format!("cannot encode retained artifact manifest: {error}"))
        })?;
        manifest_bytes.push(b'\n');
        write_bytes_atomic(
            &evidence_dir.join(RETAINED_ARTIFACT_MANIFEST),
            &manifest_bytes,
            MAX_RETAINED_ARTIFACT_MANIFEST_BYTES,
        )?;
        let evidence = File::open(evidence_dir).map_err(|error| {
            ExecutorError::new(format!("cannot open retained evidence directory: {error}"))
        })?;
        evidence.sync_all().map_err(|error| {
            ExecutorError::new(format!("cannot sync retained evidence directory: {error}"))
        })?;
        Ok(manifest
            .artifacts
            .iter()
            .map(|tree| RetainedArtifactReceipt {
                name: tree.name.clone(),
                size: tree.size,
                files: tree.files,
                sha256: tree.sha256.clone(),
            })
            .collect())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&retained);
        let _ = fs::remove_file(evidence_dir.join(RETAINED_ARTIFACT_MANIFEST));
    }
    result
}

fn copy_tree(
    source: &File,
    destination: &Path,
    prefix: &str,
    maximum: u64,
    total: &mut u64,
    entries: &mut Vec<RetainedArtifactFile>,
) -> Result<(), ExecutorError> {
    let before = source.metadata().map_err(|error| {
        ExecutorError::new(format!(
            "cannot inspect retained artifact directory: {error}"
        ))
    })?;
    let names = directory_names(source)?;
    for name in &names {
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if relative.len() > 512
            || relative.contains('\\')
            || relative.chars().any(|value| value.is_control())
        {
            return Err(ExecutorError::new(
                "retained artifact file path exceeds its safe bound",
            ));
        }
        match open_directory_at(source, name) {
            Ok(child) => {
                let child_destination = destination.join(name);
                fs::create_dir(&child_destination).map_err(|error| {
                    ExecutorError::new(format!(
                        "cannot create retained artifact directory {relative:?}: {error}"
                    ))
                })?;
                fs::set_permissions(&child_destination, fs::Permissions::from_mode(0o700))
                    .map_err(|error| {
                        ExecutorError::new(format!(
                            "cannot protect retained artifact directory {relative:?}: {error}"
                        ))
                    })?;
                copy_tree(
                    &child,
                    &child_destination,
                    &relative,
                    maximum,
                    total,
                    entries,
                )?;
            }
            Err(error) if error == rustix::io::Errno::NOTDIR => {
                if entries.len() >= MAX_RETAINED_ARTIFACT_FILES {
                    return Err(ExecutorError::new(format!(
                        "retained artifact exceeds {MAX_RETAINED_ARTIFACT_FILES} files"
                    )));
                }
                let file = open_regular_file_at(source, name)?;
                let destination_file = destination.join(name);
                let (size, sha256) = copy_regular_file(&file, &destination_file, maximum, *total)?;
                *total = total
                    .checked_add(size)
                    .ok_or_else(|| ExecutorError::new("retained artifact byte count overflow"))?;
                entries.push(RetainedArtifactFile {
                    path: relative,
                    size,
                    sha256,
                });
            }
            Err(_) => {
                return Err(ExecutorError::new(format!(
                    "retained artifact entry {relative:?} is not a regular file or directory"
                )));
            }
        }
    }
    if directory_names(source)? != names {
        return Err(ExecutorError::new(
            "retained artifact directory changed while it was copied",
        ));
    }
    let after = source.metadata().map_err(|error| {
        ExecutorError::new(format!(
            "cannot restat retained artifact directory: {error}"
        ))
    })?;
    if directory_identity(&before) != directory_identity(&after) {
        return Err(ExecutorError::new(
            "retained artifact directory changed while it was copied",
        ));
    }
    let destination_directory = File::open(destination).map_err(|error| {
        ExecutorError::new(format!("cannot open retained artifact directory: {error}"))
    })?;
    destination_directory.sync_all().map_err(|error| {
        ExecutorError::new(format!("cannot sync retained artifact directory: {error}"))
    })?;
    Ok(())
}

fn copy_regular_file(
    source: &File,
    destination: &Path,
    maximum: u64,
    prior_total: u64,
) -> Result<(u64, String), ExecutorError> {
    let before = source.metadata().map_err(|error| {
        ExecutorError::new(format!("cannot inspect retained artifact file: {error}"))
    })?;
    if !before.is_file() {
        return Err(ExecutorError::new(
            "retained artifact entry is not a regular file",
        ));
    }
    if prior_total
        .checked_add(before.len())
        .is_none_or(|size| size > maximum)
    {
        return Err(ExecutorError::new(format!(
            "retained artifact exceeds its {maximum} byte limit"
        )));
    }
    let mut input = source.try_clone().map_err(|error| {
        ExecutorError::new(format!("cannot clone retained artifact file: {error}"))
    })?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|error| {
            ExecutorError::new(format!("cannot create retained artifact file: {error}"))
        })?;
    let mut digest = Sha256::new();
    let mut copied = 0_u64;
    let mut block = vec![0_u8; COPY_BLOCK_BYTES];
    loop {
        let read = input.read(&mut block).map_err(|error| {
            ExecutorError::new(format!("cannot read retained artifact file: {error}"))
        })?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| ExecutorError::new("retained artifact byte count overflow"))?;
        if prior_total
            .checked_add(copied)
            .is_none_or(|size| size > maximum)
        {
            return Err(ExecutorError::new(format!(
                "retained artifact exceeds its {maximum} byte limit"
            )));
        }
        digest.update(&block[..read]);
        output.write_all(&block[..read]).map_err(|error| {
            ExecutorError::new(format!("cannot write retained artifact file: {error}"))
        })?;
    }
    output.sync_all().map_err(|error| {
        ExecutorError::new(format!("cannot sync retained artifact file: {error}"))
    })?;
    let after = source.metadata().map_err(|error| {
        ExecutorError::new(format!("cannot restat retained artifact file: {error}"))
    })?;
    if identity(&before) != identity(&after) || copied != before.len() {
        return Err(ExecutorError::new(
            "retained artifact file changed while it was copied",
        ));
    }
    Ok((copied, lower_hex(&digest.finalize())))
}

fn tree_digest(entries: &[RetainedArtifactFile]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"devcoordinator2-retained-artifact-tree-v1\0");
    for entry in entries {
        digest.update(entry.path.as_bytes());
        digest.update(b"\0");
        digest.update(entry.size.to_string().as_bytes());
        digest.update(b"\0");
        digest.update(entry.sha256.as_bytes());
        digest.update(b"\0");
    }
    lower_hex(&digest.finalize())
}

fn open_directory_path(path: &Path, label: &str) -> Result<File, ExecutorError> {
    unix_fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| ExecutorError::new(format!("cannot open {label}: {error}")))
}

fn open_relative_directory(root: &File, relative: &str) -> Result<File, ExecutorError> {
    let mut current = root.try_clone().map_err(|error| {
        ExecutorError::new(format!("cannot clone worktree descriptor: {error}"))
    })?;
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            return Err(ExecutorError::new("retained artifact path is invalid"));
        };
        let name = component
            .to_str()
            .ok_or_else(|| ExecutorError::new("retained artifact path is not UTF-8"))?;
        current = open_directory_at(&current, name).map_err(|error| {
            ExecutorError::new(format!(
                "cannot open retained artifact directory {relative:?}: {error}"
            ))
        })?;
    }
    Ok(current)
}

fn open_directory_at(parent: &File, name: &str) -> Result<File, rustix::io::Errno> {
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
}

fn open_regular_file_at(parent: &File, name: &str) -> Result<File, ExecutorError> {
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        ExecutorError::new(format!(
            "cannot open retained artifact file {name:?}: {error}"
        ))
    })
}

fn directory_names(directory: &File) -> Result<Vec<String>, ExecutorError> {
    let mut reader = Dir::read_from(directory).map_err(|error| {
        ExecutorError::new(format!("cannot list retained artifact directory: {error}"))
    })?;
    let mut names = Vec::new();
    for entry in &mut reader {
        let entry = entry.map_err(|error| {
            ExecutorError::new(format!("cannot read retained artifact directory: {error}"))
        })?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::str::from_utf8(bytes)
            .map_err(|_| ExecutorError::new("retained artifact name is not UTF-8"))?
            .to_owned();
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(|value| value.is_control())
        {
            return Err(ExecutorError::new("retained artifact name is invalid"));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn directory_identity(metadata: &fs::Metadata) -> (u64, i64, i64) {
    (metadata.ino(), metadata.mtime(), metadata.mtime_nsec())
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ExecutorError> {
    let mut payload = serde_json::to_vec(value)
        .map_err(|error| ExecutorError::new(format!("cannot encode report: {error}")))?;
    payload.push(b'\n');
    write_bytes_atomic(path, &payload, MAX_REPORT_BYTES)
}

pub fn write_bytes_atomic(
    path: &Path,
    payload: &[u8],
    maximum: usize,
) -> Result<(), ExecutorError> {
    if payload.len() > maximum {
        return Err(ExecutorError::new(format!(
            "artifact {:?} exceeds its {} byte limit",
            path.file_name().unwrap_or_default(),
            maximum
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ExecutorError::new("artifact path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| {
        ExecutorError::new(format!("cannot create artifact directory: {error}"))
    })?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("artifact");
    let temporary = parent.join(format!(".{filename}-{}-{sequence}.tmp", std::process::id()));
    let result = (|| -> Result<(), ExecutorError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| ExecutorError::new(format!("cannot create artifact: {error}")))?;
        file.write_all(payload)
            .and_then(|_| file.sync_all())
            .map_err(|error| ExecutorError::new(format!("cannot persist artifact: {error}")))?;
        fs::rename(&temporary, path)
            .map_err(|error| ExecutorError::new(format!("cannot publish artifact: {error}")))?;
        let directory = File::open(parent).map_err(|error| {
            ExecutorError::new(format!("cannot open artifact directory: {error}"))
        })?;
        directory.sync_all().map_err(|error| {
            ExecutorError::new(format!("cannot sync artifact directory: {error}"))
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .arg("-C")
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", "*");
    command
}

fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>, ExecutorError> {
    let output = git_command(root)
        .args(args)
        .output()
        .map_err(|error| ExecutorError::new(format!("cannot inventory source: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(ExecutorError::new(format!(
            "cannot inventory source: {}",
            bounded(&detail, 512)
        )));
    }
    if output.stdout.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(ExecutorError::new(
            "repository source inventory exceeds 16 MiB",
        ));
    }
    Ok(output.stdout)
}

fn matches_index_blob(root: &Path, relative: &str, index: &[u8]) -> Result<bool, ExecutorError> {
    let indexed = index.split(|byte| *byte == 0).find_map(|entry| {
        let tab = entry.iter().position(|byte| *byte == b'\t')?;
        if &entry[tab + 1..] != relative.as_bytes() {
            return None;
        }
        let header = std::str::from_utf8(&entry[..tab]).ok()?;
        let mut fields = header.split(' ');
        let mode = fields.next()?;
        let object = fields.next()?;
        if fields.next()? != "0" {
            return None;
        }
        Some((mode, object))
    });
    let Some((mode, object)) = indexed else {
        return Ok(false);
    };
    validate_relative(relative)?;
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ExecutorError::new(format!(
                "cannot inspect source candidate: {error}"
            )));
        }
    };
    let hash = if metadata.is_file() && matches!(mode, "100644" | "100755") {
        let executable = metadata.permissions().mode() & 0o111 != 0;
        if executable != (mode == "100755") {
            return Ok(false);
        }
        git_output(root, &["hash-object", "--no-filters", "--", relative])?
    } else if metadata.file_type().is_symlink() && mode == "120000" {
        // Hash the link's own bytes; never follow its destination for identity.
        let target = fs::read_link(&path)
            .map_err(|error| ExecutorError::new(format!("cannot read source link: {error}")))?;
        let mut child = git_command(root)
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| ExecutorError::new(format!("cannot hash source link: {error}")))?;
        let written = child
            .stdin
            .take()
            .ok_or_else(|| ExecutorError::new("missing source hash input"))?
            .write_all(target.as_os_str().as_encoded_bytes());
        let output = child
            .wait_with_output()
            .map_err(|error| ExecutorError::new(format!("cannot finish source hash: {error}")))?;
        written
            .map_err(|error| ExecutorError::new(format!("cannot supply source link: {error}")))?;
        if !output.status.success() {
            return Err(ExecutorError::new("cannot hash source link"));
        }
        output.stdout
    } else {
        return Ok(false);
    };
    let after = fs::symlink_metadata(&path)
        .map_err(|error| ExecutorError::new(format!("cannot restat source candidate: {error}")))?;
    if identity(&metadata) != identity(&after)
        || metadata.permissions().mode() != after.permissions().mode()
    {
        return Err(ExecutorError::new(
            "source candidate changed during blob comparison",
        ));
    }
    Ok(hash.strip_suffix(b"\n").unwrap_or(&hash) == object.as_bytes())
}

fn decode_paths(payload: Vec<u8>) -> Result<Vec<String>, ExecutorError> {
    let mut paths = Vec::new();
    for raw in payload
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let value = std::str::from_utf8(raw)
            .map_err(|_| ExecutorError::new("repository contains a non-UTF-8 source path"))?;
        validate_relative(value)?;
        paths.push(value.to_owned());
    }
    let unique: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    if unique.len() != paths.len() {
        return Err(ExecutorError::new(
            "repository source inventory contains duplicate paths",
        ));
    }
    Ok(paths)
}

fn hash_source_path(root: &Path, relative: &str, digest: &mut Sha256) -> Result<(), ExecutorError> {
    validate_relative(relative)?;
    digest.update(relative.as_bytes());
    digest.update(b"\0");
    let path = root.join(relative);
    let before = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            digest.update(b"missing\0");
            return Ok(());
        }
        Err(error) => {
            return Err(ExecutorError::new(format!(
                "cannot inspect source path {relative:?}: {error}"
            )));
        }
    };
    digest.update(format!("{:o}\0", before.permissions().mode() & 0o7777).as_bytes());
    if before.file_type().is_symlink() {
        let target = fs::read_link(&path).map_err(|error| {
            ExecutorError::new(format!("cannot read source symlink {relative:?}: {error}"))
        })?;
        digest.update(b"symlink\0");
        digest.update(target.as_os_str().as_encoded_bytes());
        digest.update(b"\0");
        return Ok(());
    }
    if before.is_dir() {
        digest.update(b"gitlink\0");
        return Ok(());
    }
    if !before.is_file() {
        return Err(ExecutorError::new(format!(
            "source path {relative:?} is not a regular file"
        )));
    }
    let mut file = File::open(&path).map_err(|error| {
        ExecutorError::new(format!("cannot open source path {relative:?}: {error}"))
    })?;
    let mut block = vec![0_u8; COPY_BLOCK_BYTES];
    loop {
        let read = file.read(&mut block).map_err(|error| {
            ExecutorError::new(format!("cannot read source path {relative:?}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        digest.update(&block[..read]);
    }
    let after = file.metadata().map_err(|error| {
        ExecutorError::new(format!("cannot restat source path {relative:?}: {error}"))
    })?;
    if identity(&before) != identity(&after) {
        return Err(ExecutorError::new(format!(
            "source path {relative:?} changed while hashing"
        )));
    }
    Ok(())
}

fn artifact_receipt(root: &Path, relative: &str) -> Result<ArtifactReceipt, ExecutorError> {
    validate_relative(relative)?;
    let path = root.join(relative);
    let before = fs::symlink_metadata(&path).map_err(|error| {
        ExecutorError::new(format!(
            "declared artifact {relative:?} is unavailable: {error}"
        ))
    })?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(ExecutorError::new(format!(
            "declared artifact {relative:?} is not a regular file"
        )));
    }
    let canonical = path.canonicalize().map_err(|error| {
        ExecutorError::new(format!("cannot resolve artifact {relative:?}: {error}"))
    })?;
    if !canonical.starts_with(root) {
        return Err(ExecutorError::new(format!(
            "declared artifact {relative:?} escapes the worktree"
        )));
    }
    let mut file = File::open(&canonical).map_err(|error| {
        ExecutorError::new(format!("cannot open artifact {relative:?}: {error}"))
    })?;
    let mut digest = Sha256::new();
    let mut block = vec![0_u8; COPY_BLOCK_BYTES];
    loop {
        let read = file.read(&mut block).map_err(|error| {
            ExecutorError::new(format!("cannot read artifact {relative:?}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        digest.update(&block[..read]);
    }
    let after = file.metadata().map_err(|error| {
        ExecutorError::new(format!("cannot restat artifact {relative:?}: {error}"))
    })?;
    if identity(&before) != identity(&after) {
        return Err(ExecutorError::new(format!(
            "declared artifact {relative:?} changed while hashing"
        )));
    }
    Ok(ArtifactReceipt {
        path: relative.into(),
        size: before.len(),
        sha256: lower_hex(&digest.finalize()),
    })
}

fn validate_relative(value: &str) -> Result<(), ExecutorError> {
    let path = PathBuf::from(value);
    if value.is_empty()
        || path.is_absolute()
        || value.contains('\\')
        || value.as_bytes().contains(&0)
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ExecutorError::new(format!(
            "path {value:?} is not normalized repository-relative"
        )));
    }
    Ok(())
}

fn identity(metadata: &fs::Metadata) -> (u64, u64, i64, i64) {
    (
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

fn bounded(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "dc2-executor-evidence-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary directory");
        path
    }

    #[test]
    fn artifact_receipt_hashes_regular_file_and_rejects_symlink() {
        let root = temporary("receipt");
        fs::write(root.join("artifact.txt"), b"evidence").expect("artifact");
        let receipts = artifact_receipts(&root, &["artifact.txt".into()]).expect("receipt");
        assert_eq!(receipts[0].size, 8);
        assert_eq!(receipts[0].sha256.len(), 64);
        symlink("artifact.txt", root.join("link.txt")).expect("symlink");
        assert!(artifact_receipts(&root, &["link.txt".into()]).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn retained_artifact_tree_is_copied_hash_bound_and_manifested() {
        let root = temporary("retained-tree");
        let source = root.join("browser-evidence");
        fs::create_dir_all(source.join("nested")).expect("source directories");
        fs::write(source.join("result.json"), b"{\"ok\":true}\n").expect("result");
        fs::write(source.join("nested/screenshot.png"), b"png-bytes").expect("image");
        let evidence = root.join("run/check/evidence");
        let receipts = retain_artifact_trees(
            &root,
            &evidence,
            &RetainedArtifactIdentity {
                run_id: "run-1",
                test: "complete",
                check: "browser",
                requested_tier: ValidationTier::Release,
                readiness_eligible: true,
                proof: ProofKind::Complete,
                source_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                config_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
            &[RetainedArtifactSpec {
                name: "production".into(),
                path: "browser-evidence".into(),
                max_bytes: 1024,
            }],
        )
        .expect("retained tree");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].name, "production");
        assert_eq!(receipts[0].files, 2);
        assert_eq!(
            fs::read(evidence.join("retained/production/nested/screenshot.png"))
                .expect("retained image"),
            b"png-bytes"
        );
        let manifest: RetainedArtifactManifest = serde_json::from_slice(
            &fs::read(evidence.join(RETAINED_ARTIFACT_MANIFEST)).expect("manifest"),
        )
        .expect("typed manifest");
        assert_eq!(manifest.run_id, "run-1");
        assert_eq!(manifest.check, "browser");
        assert_eq!(manifest.artifacts[0].sha256, receipts[0].sha256);
        assert_eq!(
            manifest.artifacts[0]
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["nested/screenshot.png", "result.json"]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn retained_artifact_tree_rejects_links_empty_and_oversized_sources() {
        let root = temporary("retained-invalid");
        let source = root.join("evidence");
        fs::create_dir(&source).expect("source");
        let empty_result = retain_artifact_trees(
            &root,
            &root.join("run-empty/evidence"),
            &RetainedArtifactIdentity {
                run_id: "run-empty",
                test: "complete",
                check: "browser",
                requested_tier: ValidationTier::Development,
                readiness_eligible: false,
                proof: ProofKind::Complete,
                source_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                config_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
            &[RetainedArtifactSpec {
                name: "empty".into(),
                path: "evidence".into(),
                max_bytes: 10,
            }],
        );
        assert!(empty_result.is_err());

        fs::write(source.join("large.bin"), b"12345678901").expect("large");
        let oversized = retain_artifact_trees(
            &root,
            &root.join("run-large/evidence"),
            &RetainedArtifactIdentity {
                run_id: "run-large",
                test: "complete",
                check: "browser",
                requested_tier: ValidationTier::Development,
                readiness_eligible: false,
                proof: ProofKind::Complete,
                source_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                config_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
            &[RetainedArtifactSpec {
                name: "large".into(),
                path: "evidence".into(),
                max_bytes: 10,
            }],
        );
        assert!(oversized.is_err());

        fs::remove_file(source.join("large.bin")).expect("remove large");
        fs::write(root.join("outside"), b"outside").expect("outside");
        symlink("../outside", source.join("link")).expect("link");
        let linked = retain_artifact_trees(
            &root,
            &root.join("run-link/evidence"),
            &RetainedArtifactIdentity {
                run_id: "run-link",
                test: "complete",
                check: "browser",
                requested_tier: ValidationTier::Development,
                readiness_eligible: false,
                proof: ProofKind::Complete,
                source_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                config_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
            &[RetainedArtifactSpec {
                name: "linked".into(),
                path: "evidence".into(),
                max_bytes: 100,
            }],
        );
        assert!(linked.is_err());
        fs::remove_file(source.join("link")).expect("remove link");
        assert!(
            Command::new("mkfifo")
                .arg(source.join("fifo"))
                .status()
                .expect("mkfifo")
                .success()
        );
        let special = retain_artifact_trees(
            &root,
            &root.join("run-special/evidence"),
            &RetainedArtifactIdentity {
                run_id: "run-special",
                test: "complete",
                check: "browser",
                requested_tier: ValidationTier::Development,
                readiness_eligible: false,
                proof: ProofKind::Complete,
                source_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                config_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            },
            &[RetainedArtifactSpec {
                name: "special".into(),
                path: "evidence".into(),
                max_bytes: 100,
            }],
        );
        assert!(special.is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn atomic_json_write_is_bounded() {
        let root = temporary("atomic");
        let path = root.join("report.json");
        write_json_atomic(&path, &serde_json::json!({"schema": 2})).expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{\"schema\":2}\n");
        let oversized = vec![b'x'; MAX_REPORT_BYTES + 1];
        assert!(write_bytes_atomic(&path, &oversized, MAX_REPORT_BYTES).is_err());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
