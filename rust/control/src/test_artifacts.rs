//! Hash-bound, bounded access to executor-retained artifact trees.
//!
//! The executor writes one immutable manifest beside a successful check. This
//! service revalidates that manifest and the schema-2 run metadata, opens every
//! path component relative to held directory descriptors without following
//! links, and returns only public identities plus an explicitly requested file
//! chunk. Storage and repository paths never appear in result or error text.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use devcoordinator2_api::params::{
    ArtifactCatalog as ArtifactCatalogParams, ArtifactFile as ArtifactFileParams,
    ValidationTier as ApiValidationTier,
};
use devcoordinator2_api::results::{
    ArtifactCatalog, ArtifactChunk, ArtifactEntry, ArtifactSummary, ProofKind as ApiProofKind,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use devcoordinator2_executor_core::{RunLogMetadata, protocol};
use protocol::{
    MAX_RETAINED_ARTIFACT_BYTES, MAX_RETAINED_ARTIFACT_FILES, MAX_RETAINED_ARTIFACT_MANIFEST_BYTES,
    MAX_RETAINED_ARTIFACT_TOTAL_BYTES, MAX_RETAINED_ARTIFACTS, ProofKind, RetainedArtifactReceipt,
    RunStatus, ValidationTier,
};
use regex::Regex;
use rusqlite::OptionalExtension;
use rustix::fs::{self as unix_fs, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::access::Caller;
use crate::database::{Database, DatabaseError};
use crate::repository::Registry;

const MANIFEST_NAME: &str = "retained-artifacts.json";
const MANIFEST_KIND: &str = "devcoordinator2-retained-artifact-trees";
const MANIFEST_SCHEMA: u8 = 1;
const RUN_METADATA_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: u32 = 180 * 1024;
const MAX_PAGE: u16 = 100;
const MAX_RELATIVE_PATH_BYTES: usize = 512;
const HASH_BLOCK_BYTES: usize = 1024 * 1024;
const MAX_VERIFIED_KEYS: usize = MAX_RETAINED_ARTIFACT_FILES * MAX_RETAINED_ARTIFACTS;

#[derive(Clone)]
pub struct TestArtifactService {
    database: Database,
    registry: Registry,
    verified: Arc<Mutex<HashSet<VerifiedKey>>>,
}

impl TestArtifactService {
    pub fn new(database: Database, registry: Registry) -> Self {
        Self {
            database,
            registry,
            verified: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// List artifact summaries or one verified, bounded page of file receipts.
    pub fn catalog(
        &self,
        params: ArtifactCatalogParams,
        caller: &Caller,
    ) -> Result<ArtifactCatalog, ProtocolError> {
        let resolved = self.resolve(Path::new(&params.path), caller)?;
        validate_run_id(&params.run_id)?;
        validate_check_name(&params.check, "check")?;
        if let Some(artifact) = params.artifact.as_deref() {
            validate_check_name(artifact, "artifact")?;
        }
        if let Some(digest) = params.manifest_sha256.as_deref() {
            validate_digest(digest, "manifest_sha256")?;
        }
        if params.offset as usize > MAX_RETAINED_ARTIFACT_FILES {
            return Err(invalid_argument("'offset' must be an integer in 0..4096"));
        }
        if params.limit == 0 || params.limit > MAX_PAGE {
            return Err(invalid_argument("'limit' must be an integer in 1..100"));
        }

        let verified = self.manifest(&resolved.worktree, &params.run_id, &params.check)?;
        if params
            .manifest_sha256
            .as_deref()
            .is_some_and(|expected| expected != verified.manifest_sha256)
        {
            return Err(tampered("The retained artifact manifest changed."));
        }

        let selected = if let Some(name) = params.artifact.as_deref() {
            let artifact = verified
                .manifest
                .artifacts
                .iter()
                .find(|artifact| artifact.name == name)
                .ok_or_else(|| not_found("The selected retained artifact is unavailable."))?;
            self.verify_tree(
                &resolved.worktree,
                &resolved.worktree_id,
                &params.run_id,
                &params.check,
                artifact,
            )?;
            Some(artifact)
        } else {
            None
        };

        let entries = selected.map_or(&[][..], |artifact| artifact.entries.as_slice());
        let offset = usize::try_from(params.offset)
            .map_err(|_| invalid_argument("'offset' exceeds the artifact file count"))?;
        if offset > entries.len() {
            return Err(invalid_argument("'offset' exceeds the artifact file count"));
        }
        let end = offset
            .saturating_add(usize::from(params.limit))
            .min(entries.len());
        let page = entries[offset..end].to_vec();
        let next_offset = (end < entries.len())
            .then(|| u32::try_from(end).expect("artifact file count is bounded to u32"));
        let artifact = selected.map(ManifestArtifact::summary);
        let artifacts = verified
            .manifest
            .artifacts
            .iter()
            .map(ManifestArtifact::summary)
            .collect();

        Ok(ArtifactCatalog {
            repository_id: resolved.repository_id,
            worktree_id: resolved.worktree_id,
            run_id: params.run_id,
            check: params.check,
            manifest_sha256: verified.manifest_sha256,
            test: verified.manifest.test,
            requested_tier: api_tier(verified.manifest.requested_tier),
            readiness_eligible: verified.manifest.readiness_eligible,
            proof: api_proof(verified.manifest.proof),
            source_sha256: verified.manifest.source_sha256,
            config_sha256: verified.manifest.config_sha256,
            run_status: run_status(verified.run.status).to_owned(),
            run_complete: verified.run.complete,
            run_finished_at_epoch_ms: verified.run.finished_at_epoch_ms,
            run_metadata_sha256: verified.run_metadata_sha256,
            artifacts,
            artifact,
            entries: page,
            next_offset,
        })
    }

    pub(crate) fn delivery_run_start(
        &self,
        path: &str,
        run_id: &str,
        check: &str,
        expected_metadata: &str,
        caller: &Caller,
    ) -> Result<u64, ProtocolError> {
        let resolved = self.resolve(Path::new(path), caller)?;
        validate_run_id(run_id)?;
        validate_check_name(check, "check")?;
        let verified = self.manifest(&resolved.worktree, run_id, check)?;
        if verified.run_metadata_sha256 != expected_metadata {
            return Err(tampered(
                "Retained run metadata changed during delivery verification.",
            ));
        }
        Ok(verified.run.started_at_epoch_ms)
    }

    /// Read one exact chunk after revalidating its manifest and file identity.
    pub fn file(
        &self,
        params: ArtifactFileParams,
        caller: &Caller,
    ) -> Result<ArtifactChunk, ProtocolError> {
        let resolved = self.resolve(Path::new(&params.path), caller)?;
        validate_run_id(&params.run_id)?;
        validate_check_name(&params.check, "check")?;
        validate_check_name(&params.artifact, "artifact")?;
        validate_relative_file(&params.file).map_err(|_| invalid_argument("'file' is invalid"))?;
        validate_digest(&params.manifest_sha256, "manifest_sha256")?;
        if params.offset > MAX_RETAINED_ARTIFACT_BYTES {
            return Err(invalid_argument(
                "'offset' must be an integer in 0..1073741824",
            ));
        }
        if params.max_bytes == 0 || params.max_bytes > MAX_CHUNK_BYTES {
            return Err(invalid_argument(
                "'max_bytes' must be an integer in 1..184320",
            ));
        }

        let verified = self.manifest(&resolved.worktree, &params.run_id, &params.check)?;
        if verified.manifest_sha256 != params.manifest_sha256 {
            return Err(tampered("The retained artifact manifest changed."));
        }
        let artifact = verified
            .manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.name == params.artifact)
            .ok_or_else(|| not_found("The selected retained artifact is unavailable."))?;
        let entry = artifact
            .entries
            .iter()
            .find(|entry| entry.path == params.file)
            .ok_or_else(|| not_found("The selected retained artifact file is unavailable."))?;
        if params.offset > entry.size {
            return Err(invalid_argument("'offset' is beyond the retained file"));
        }
        let (file, before) = self.verified_file(
            &resolved.worktree,
            &resolved.worktree_id,
            &params.run_id,
            &params.check,
            &params.artifact,
            entry,
        )?;
        let remaining = entry.size - params.offset;
        let wanted = usize::try_from(remaining.min(u64::from(params.max_bytes)))
            .expect("chunk size is bounded to usize");
        let block = read_exact_at(&file, params.offset, wanted)
            .map_err(|_| tampered("The retained artifact changed while read."))?;
        let after = file
            .metadata()
            .map_err(|_| tampered("The retained artifact changed while read."))?;
        if FileIdentity::from_metadata(&after) != before {
            return Err(tampered("The retained artifact changed while read."));
        }
        let bytes = u32::try_from(block.len()).expect("artifact chunk is bounded to u32");
        let next = params.offset + u64::from(bytes);
        Ok(ArtifactChunk {
            run_id: params.run_id,
            check: params.check,
            artifact: params.artifact,
            file: params.file,
            sha256: entry.sha256.clone(),
            total_bytes: entry.size,
            offset: params.offset,
            bytes,
            base64: base64_standard(&block),
            next_offset: (next < entry.size).then_some(next),
        })
    }

    fn manifest(
        &self,
        worktree: &Path,
        run_id: &str,
        check: &str,
    ) -> Result<VerifiedManifest, ProtocolError> {
        let run = open_run_directory(worktree, run_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::TestArtifactExpired,
                "The selected retained artifact is unavailable or has expired.",
            )
        })?;
        let run_raw = read_bounded_regular(&run, "run.json", RUN_METADATA_BYTES)
            .map_err(|_| not_found("The selected check has no retained artifacts."))?;
        let evidence = open_check_evidence(&run, check)
            .map_err(|_| not_found("The selected check has no retained artifacts."))?;
        let manifest_raw = read_bounded_regular(
            &evidence,
            MANIFEST_NAME,
            MAX_RETAINED_ARTIFACT_MANIFEST_BYTES,
        )
        .map_err(|_| not_found("The selected check has no retained artifacts."))?;

        let manifest: RetainedManifest = serde_json::from_slice(&manifest_raw.bytes)
            .map_err(|_| tampered("The retained artifact manifest is invalid."))?;
        validate_manifest(&manifest, run_id, check)
            .map_err(|_| tampered("The retained artifact manifest is invalid."))?;
        let run: RunLogMetadata = serde_json::from_slice(&run_raw.bytes)
            .map_err(|_| tampered("The retained artifact manifest is invalid."))?;
        validate_run(&run, run_id, &manifest.test)
            .map_err(|_| tampered("The retained artifact manifest is invalid."))?;
        Ok(VerifiedManifest {
            manifest_sha256: sha256_hex(&manifest_raw.bytes),
            run_metadata_sha256: sha256_hex(&run_raw.bytes),
            manifest,
            run,
        })
    }

    fn verify_tree(
        &self,
        worktree: &Path,
        worktree_id: &str,
        run_id: &str,
        check: &str,
        artifact: &ManifestArtifact,
    ) -> Result<(), ProtocolError> {
        for entry in &artifact.entries {
            self.verified_file(worktree, worktree_id, run_id, check, &artifact.name, entry)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn verified_file(
        &self,
        worktree: &Path,
        worktree_id: &str,
        run_id: &str,
        check: &str,
        artifact: &str,
        entry: &ArtifactEntry,
    ) -> Result<(File, FileIdentity), ProtocolError> {
        let file = open_artifact_file(worktree, run_id, check, artifact, &entry.path)
            .map_err(|_| tampered("A retained artifact file is unavailable."))?;
        let before_metadata = file
            .metadata()
            .map_err(|_| tampered("A retained artifact file is unavailable."))?;
        let before = FileIdentity::from_metadata(&before_metadata);
        if !before_metadata.is_file() || before.size != entry.size {
            return Err(tampered("A retained artifact file no longer matches."));
        }
        let key = VerifiedKey {
            worktree_id: worktree_id.to_owned(),
            run_id: run_id.to_owned(),
            check: check.to_owned(),
            artifact: artifact.to_owned(),
            file: entry.path.clone(),
            sha256: entry.sha256.clone(),
            identity: before,
        };
        let cached = self
            .verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&key);
        if !cached {
            let digest = hash_open_file(&file, entry.size)
                .map_err(|_| tampered("A retained artifact file no longer matches."))?;
            let after = file
                .metadata()
                .map_err(|_| tampered("A retained artifact file no longer matches."))?;
            if FileIdentity::from_metadata(&after) != before || digest != entry.sha256 {
                return Err(tampered("A retained artifact file no longer matches."));
            }
            let mut verified = self
                .verified
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if verified.len() >= MAX_VERIFIED_KEYS {
                verified.clear();
            }
            verified.insert(key);
        }
        Ok((file, before))
    }

    fn resolve(&self, path: &Path, caller: &Caller) -> Result<ResolvedWorktree, ProtocolError> {
        if !path.is_absolute() {
            return Err(invalid_argument("path must be absolute"));
        }
        if caller.identity.is_some() {
            let requested = path.to_string_lossy().into_owned();
            return self
                .database
                .call(move |connection| {
                    connection
                        .query_row(
                            "SELECT worktree_path,worktree_id,repository_id FROM worktrees WHERE worktree_path=?1",
                            [&requested],
                            |row| {
                                Ok(ResolvedWorktree {
                                    worktree: PathBuf::from(row.get::<_, String>(0)?),
                                    worktree_id: row.get(1)?,
                                    repository_id: row.get(2)?,
                                })
                            },
                        )
                        .optional()
                        .map_err(DatabaseError::from)
                })
                .map_err(database_error)?
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryNotFound,
                        "No registered worktree matches this request.",
                    )
                });
        }
        let status = self
            .registry
            .repository_status(path, Some((caller.uid, caller.gid)))
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    "The worktree is not registered.",
                )
            })?;
        let worktree = status
            .worktrees
            .iter()
            .find(|worktree| worktree.worktree_id == status.worktree_id)
            .map(|worktree| PathBuf::from(&worktree.worktree_path))
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    "The worktree is not registered.",
                )
            })?;
        Ok(ResolvedWorktree {
            worktree,
            repository_id: status.repository_id,
            worktree_id: status.worktree_id,
        })
    }
}

#[derive(Clone, Debug)]
struct ResolvedWorktree {
    worktree: PathBuf,
    repository_id: String,
    worktree_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedManifest {
    schema: u8,
    kind: String,
    run_id: String,
    test: String,
    check: String,
    requested_tier: ValidationTier,
    readiness_eligible: bool,
    proof: ProofKind,
    source_sha256: String,
    config_sha256: String,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestArtifact {
    name: String,
    size: u64,
    files: u32,
    sha256: String,
    entries: Vec<ArtifactEntry>,
}

impl ManifestArtifact {
    fn receipt(&self) -> RetainedArtifactReceipt {
        RetainedArtifactReceipt {
            name: self.name.clone(),
            size: self.size,
            files: self.files,
            sha256: self.sha256.clone(),
        }
    }

    fn summary(&self) -> ArtifactSummary {
        let receipt = self.receipt();
        ArtifactSummary {
            name: receipt.name,
            size: receipt.size,
            files: receipt.files,
            sha256: receipt.sha256,
        }
    }
}

#[derive(Debug)]
struct VerifiedManifest {
    manifest_sha256: String,
    run_metadata_sha256: String,
    manifest: RetainedManifest,
    run: RunLogMetadata,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct VerifiedKey {
    worktree_id: String,
    run_id: String,
    check: String,
    artifact: String,
    file: String,
    sha256: String,
    identity: FileIdentity,
}

#[derive(Debug)]
struct BoundedFile {
    bytes: Vec<u8>,
}

fn run_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("constant run regex")
    })
}

fn check_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").expect("constant check regex"))
}

fn test_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,31}$").expect("constant test regex"))
}

fn digest_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").expect("constant digest regex"))
}

fn validate_run_id(value: &str) -> Result<(), ProtocolError> {
    if run_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument("'run_id' is invalid"))
    }
}

fn validate_check_name(value: &str, label: &str) -> Result<(), ProtocolError> {
    if check_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument(format!("'{label}' is invalid")))
    }
}

fn validate_digest(value: &str, label: &str) -> Result<(), ProtocolError> {
    if digest_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument(format!("'{label}' is invalid")))
    }
}

fn validate_relative_file(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > MAX_RELATIVE_PATH_BYTES
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(());
    }
    Ok(())
}

fn validate_manifest(manifest: &RetainedManifest, run_id: &str, check: &str) -> Result<(), ()> {
    if manifest.schema != MANIFEST_SCHEMA
        || manifest.kind != MANIFEST_KIND
        || manifest.run_id != run_id
        || manifest.check != check
        || !test_regex().is_match(&manifest.test)
        || !digest_regex().is_match(&manifest.source_sha256)
        || !digest_regex().is_match(&manifest.config_sha256)
        || manifest.artifacts.is_empty()
        || manifest.artifacts.len() > MAX_RETAINED_ARTIFACTS
    {
        return Err(());
    }
    let mut names = HashSet::new();
    let mut combined = 0u64;
    for artifact in &manifest.artifacts {
        if !check_regex().is_match(&artifact.name)
            || !names.insert(artifact.name.as_str())
            || artifact.size > MAX_RETAINED_ARTIFACT_BYTES
            || artifact.files == 0
            || usize::try_from(artifact.files).ok() != Some(artifact.entries.len())
            || artifact.entries.len() > MAX_RETAINED_ARTIFACT_FILES
            || !digest_regex().is_match(&artifact.sha256)
        {
            return Err(());
        }
        let mut total = 0u64;
        let mut previous: Option<&str> = None;
        for entry in &artifact.entries {
            if validate_relative_file(&entry.path).is_err()
                || previous.is_some_and(|prior| prior >= entry.path.as_str())
                || entry.size > MAX_RETAINED_ARTIFACT_BYTES
                || !digest_regex().is_match(&entry.sha256)
            {
                return Err(());
            }
            total = total.checked_add(entry.size).ok_or(())?;
            if total > MAX_RETAINED_ARTIFACT_BYTES {
                return Err(());
            }
            previous = Some(&entry.path);
        }
        if total != artifact.size || retained_tree_digest(&artifact.entries) != artifact.sha256 {
            return Err(());
        }
        combined = combined.checked_add(artifact.size).ok_or(())?;
        if combined > MAX_RETAINED_ARTIFACT_TOTAL_BYTES {
            return Err(());
        }
    }
    Ok(())
}

fn validate_run(run: &RunLogMetadata, run_id: &str, test: &str) -> Result<(), ()> {
    run.validate().map_err(|_| ())?;
    if run.run_id != run_id
        || run.test != test
        || run.complete != run.finished_at_epoch_ms.is_some()
        || (run.status == RunStatus::Running) == run.complete
    {
        return Err(());
    }
    Ok(())
}

fn retained_tree_digest(entries: &[ArtifactEntry]) -> String {
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

fn sha256_hex(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

fn api_tier(value: ValidationTier) -> ApiValidationTier {
    match value {
        ValidationTier::Development => ApiValidationTier::Development,
        ValidationTier::PreMerge => ApiValidationTier::PreMerge,
        ValidationTier::Release => ApiValidationTier::Release,
    }
}

fn api_proof(value: ProofKind) -> ApiProofKind {
    match value {
        ProofKind::Complete => ApiProofKind::Complete,
        ProofKind::Selected => ApiProofKind::Selected,
        ProofKind::Retry => ApiProofKind::Retry,
    }
}

fn run_status(value: RunStatus) -> &'static str {
    match value {
        RunStatus::Running => "running",
        RunStatus::Passed => "passed",
        RunStatus::Failed => "failed",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceReadError {
    Unavailable,
    Unsafe,
}

fn open_absolute_directory(path: &Path) -> Result<File, EvidenceReadError> {
    if !path.is_absolute() {
        return Err(EvidenceReadError::Unsafe);
    }
    let mut directory = unix_fs::open(
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| EvidenceReadError::Unavailable)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = unix_fs::openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(|_| EvidenceReadError::Unavailable)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(EvidenceReadError::Unsafe);
            }
        }
    }
    Ok(directory)
}

fn open_directory(parent: &File, name: &str) -> Result<File, EvidenceReadError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(EvidenceReadError::Unsafe);
    }
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| EvidenceReadError::Unavailable)
}

fn open_run_directory(worktree: &Path, run_id: &str) -> Result<File, EvidenceReadError> {
    let mut directory = open_absolute_directory(worktree)?;
    for component in [".devcoordinator", "test", "logs", "runs", run_id] {
        directory = open_directory(&directory, component)?;
    }
    Ok(directory)
}

fn open_check_evidence(run: &File, check: &str) -> Result<File, EvidenceReadError> {
    let checks = open_directory(run, "checks")?;
    let check = open_directory(&checks, check)?;
    let leaf = open_directory(&check, "check")?;
    open_directory(&leaf, "evidence")
}

fn read_bounded_regular(
    directory: &File,
    name: &str,
    maximum: usize,
) -> Result<BoundedFile, EvidenceReadError> {
    let mut file = unix_fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| EvidenceReadError::Unavailable)?;
    let before_metadata = file
        .metadata()
        .map_err(|_| EvidenceReadError::Unavailable)?;
    if !before_metadata.is_file()
        || before_metadata.len() > u64::try_from(maximum).unwrap_or(u64::MAX)
    {
        return Err(EvidenceReadError::Unsafe);
    }
    let before = FileIdentity::from_metadata(&before_metadata);
    let capacity = usize::try_from(before.size).map_err(|_| EvidenceReadError::Unsafe)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| EvidenceReadError::Unavailable)?;
    let after = file
        .metadata()
        .map_err(|_| EvidenceReadError::Unavailable)?;
    if FileIdentity::from_metadata(&after) != before
        || u64::try_from(bytes.len()).ok() != Some(before.size)
        || bytes.len() > maximum
    {
        return Err(EvidenceReadError::Unsafe);
    }
    Ok(BoundedFile { bytes })
}

fn open_artifact_file(
    worktree: &Path,
    run_id: &str,
    check: &str,
    artifact: &str,
    relative: &str,
) -> Result<File, EvidenceReadError> {
    let run = open_run_directory(worktree, run_id)?;
    let evidence = open_check_evidence(&run, check)?;
    let retained = open_directory(&evidence, "retained")?;
    let mut current = open_directory(&retained, artifact)?;
    let mut components = relative.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_some() {
            current = open_directory(&current, component)?;
        } else {
            return unix_fs::openat(
                &current,
                component,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| EvidenceReadError::Unavailable);
        }
    }
    Err(EvidenceReadError::Unsafe)
}

fn hash_open_file(file: &File, expected_size: u64) -> Result<String, EvidenceReadError> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; HASH_BLOCK_BYTES];
    let mut offset = 0u64;
    while offset < expected_size {
        let remaining = expected_size - offset;
        let wanted = usize::try_from(remaining.min(HASH_BLOCK_BYTES as u64))
            .map_err(|_| EvidenceReadError::Unsafe)?;
        let count = file
            .read_at(&mut buffer[..wanted], offset)
            .map_err(|_| EvidenceReadError::Unavailable)?;
        if count == 0 {
            return Err(EvidenceReadError::Unsafe);
        }
        digest.update(&buffer[..count]);
        offset = offset
            .checked_add(u64::try_from(count).map_err(|_| EvidenceReadError::Unsafe)?)
            .ok_or(EvidenceReadError::Unsafe)?;
    }
    Ok(lower_hex(&digest.finalize()))
}

fn read_exact_at(file: &File, offset: u64, wanted: usize) -> Result<Vec<u8>, EvidenceReadError> {
    let mut bytes = vec![0u8; wanted];
    let mut filled = 0usize;
    while filled < wanted {
        let at = offset
            .checked_add(u64::try_from(filled).map_err(|_| EvidenceReadError::Unsafe)?)
            .ok_or(EvidenceReadError::Unsafe)?;
        let count = file
            .read_at(&mut bytes[filled..], at)
            .map_err(|_| EvidenceReadError::Unavailable)?;
        if count == 0 {
            return Err(EvidenceReadError::Unsafe);
        }
        filled += count;
    }
    Ok(bytes)
}

fn base64_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or_default();
        let third = chunk.get(2).copied().unwrap_or_default();
        output.push(char::from(ALPHABET[usize::from(first >> 2)]));
        output.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            output.push(char::from(
                ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))],
            ));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(ALPHABET[usize::from(third & 0x3f)]));
        } else {
            output.push('=');
        }
    }
    output
}

fn invalid_argument(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}

fn tampered(message: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestArtifactTampered, message)
}

fn not_found(message: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestArtifactNotFound, message)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        _ => ProtocolError::new(
            ErrorCode::InternalError,
            "Artifact repository lookup failed.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::ClientKind;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const RUN_ID: &str = "t20260903T120718Z-57067c";
    const CHECK: &str = "browser";

    struct World {
        _temporary: TempDir,
        database: Database,
        repository: PathBuf,
        evidence: PathBuf,
        manifest_sha256: String,
        artifact: ManifestArtifact,
        files: Vec<(String, Vec<u8>)>,
        service: TestArtifactService,
        caller: Caller,
    }

    impl World {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let repository = temporary.path().join("repo");
            fs::create_dir(&repository).expect("repository");
            let database =
                Database::open(temporary.path().join("authority.sqlite3")).expect("database");
            let stored = repository.to_string_lossy().into_owned();
            database
                .transaction(move |transaction| {
                    transaction.execute(
                        "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',?1,'repo','t',1,'t')",
                        [&stored],
                    )?;
                    transaction.execute(
                        "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',?1,'t','t')",
                        [&stored],
                    )?;
                    Ok(())
                })
                .expect("registered fixture");
            let evidence = repository
                .join(".devcoordinator/test/logs/runs")
                .join(RUN_ID)
                .join("checks")
                .join(CHECK)
                .join("check/evidence");
            let retained = evidence.join("retained/production");
            fs::create_dir_all(retained.join("nested")).expect("retained directories");
            let files = vec![
                ("report.json".to_owned(), b"{\"ok\":true}\n".to_vec()),
                (
                    "nested/screenshot.png".to_owned(),
                    b"fixture-png-bytes".to_vec(),
                ),
            ];
            let mut entries = Vec::new();
            for (relative, payload) in &files {
                let target = retained.join(relative);
                fs::create_dir_all(target.parent().expect("file parent")).expect("file parent");
                fs::write(&target, payload).expect("retained file");
                entries.push(ArtifactEntry {
                    path: relative.clone(),
                    size: u64::try_from(payload.len()).expect("payload size"),
                    sha256: sha256_hex(payload),
                });
            }
            entries.sort_by(|left, right| left.path.cmp(&right.path));
            let artifact = ManifestArtifact {
                name: "production".into(),
                size: entries.iter().map(|entry| entry.size).sum(),
                files: u32::try_from(entries.len()).expect("entry count"),
                sha256: retained_tree_digest(&entries),
                entries,
            };
            let manifest = RetainedManifest {
                schema: MANIFEST_SCHEMA,
                kind: MANIFEST_KIND.into(),
                run_id: RUN_ID.into(),
                test: "browser-release".into(),
                check: CHECK.into(),
                requested_tier: ValidationTier::Release,
                readiness_eligible: true,
                proof: ProofKind::Complete,
                source_sha256: "a".repeat(64),
                config_sha256: "b".repeat(64),
                artifacts: vec![artifact.clone()],
            };
            let mut manifest_bytes = serde_json::to_vec(&manifest).expect("manifest JSON");
            manifest_bytes.push(b'\n');
            fs::write(evidence.join(MANIFEST_NAME), &manifest_bytes).expect("manifest");
            let run = RunLogMetadata {
                schema: 2,
                run_id: RUN_ID.into(),
                test: "browser-release".into(),
                started_at_epoch_ms: 100,
                finished_at_epoch_ms: Some(200),
                status: RunStatus::Passed,
                complete: true,
            };
            let mut run_bytes = serde_json::to_vec(&run).expect("run JSON");
            run_bytes.push(b'\n');
            fs::write(
                repository
                    .join(".devcoordinator/test/logs/runs")
                    .join(RUN_ID)
                    .join("run.json"),
                run_bytes,
            )
            .expect("run metadata");
            let service =
                TestArtifactService::new(database.clone(), Registry::new(database.clone()));
            let caller = Caller {
                pid: 1,
                uid: 999,
                gid: 999,
                client_kind: ClientKind::Edge,
                client_session: None,
                work: None,
                identity: Some("reader@example.test".into()),
            };
            Self {
                _temporary: temporary,
                database,
                repository,
                evidence,
                manifest_sha256: sha256_hex(&manifest_bytes),
                artifact,
                files,
                service,
                caller,
            }
        }

        fn catalog_params(&self, artifact: Option<&str>) -> ArtifactCatalogParams {
            ArtifactCatalogParams {
                path: self.repository.to_string_lossy().into_owned(),
                run_id: RUN_ID.into(),
                check: CHECK.into(),
                artifact: artifact.map(str::to_owned),
                manifest_sha256: None,
                offset: 0,
                limit: 100,
            }
        }

        fn file_params(&self, file: &str, max_bytes: u32) -> ArtifactFileParams {
            ArtifactFileParams {
                path: self.repository.to_string_lossy().into_owned(),
                run_id: RUN_ID.into(),
                check: CHECK.into(),
                artifact: "production".into(),
                file: file.into(),
                manifest_sha256: self.manifest_sha256.clone(),
                offset: 0,
                max_bytes,
            }
        }
    }

    #[test]
    fn failed_run_keeps_verified_artifacts_and_failed_catalogue_status() {
        let world = World::new();
        let run_path = world
            .repository
            .join(".devcoordinator/test/logs/runs")
            .join(RUN_ID)
            .join("run.json");
        let mut run: RunLogMetadata =
            serde_json::from_slice(&fs::read(&run_path).expect("fixture run"))
                .expect("run metadata");
        run.status = RunStatus::Failed;
        fs::write(
            &run_path,
            serde_json::to_vec(&run).expect("failed metadata"),
        )
        .expect("failed run");
        let catalog = world
            .service
            .catalog(world.catalog_params(None), &world.caller)
            .expect("failed run catalogue");
        assert_eq!(catalog.run_status, "failed");
        assert!(catalog.run_complete);
        assert_eq!(catalog.artifacts, vec![world.artifact.summary()]);
        let mut params = world.catalog_params(Some("production"));
        params.manifest_sha256 = Some(catalog.manifest_sha256);
        let files = world
            .service
            .catalog(params, &world.caller)
            .expect("verified diagnostic files");
        assert_eq!(files.entries.len(), world.files.len());
    }

    #[test]
    fn catalog_pages_verified_receipts_without_private_paths() {
        let world = World::new();
        let root = world
            .service
            .catalog(world.catalog_params(None), &world.caller)
            .expect("root catalog");
        assert_eq!(root.repository_id, "r1111111111111111");
        assert_eq!(root.worktree_id, "w1111111111111111");
        assert_eq!(root.artifacts, vec![world.artifact.summary()]);
        assert_eq!(root.source_sha256, "a".repeat(64));
        assert!(root.readiness_eligible);
        assert_eq!(root.run_status, "passed");
        assert!(root.run_complete);
        let rendered = serde_json::to_string(&root).expect("public result JSON");
        assert!(!rendered.contains(&world.repository.to_string_lossy().into_owned()));

        let mut first_params = world.catalog_params(Some("production"));
        first_params.manifest_sha256 = Some(root.manifest_sha256.clone());
        first_params.limit = 1;
        let first = world
            .service
            .catalog(first_params, &world.caller)
            .expect("first page");
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.next_offset, Some(1));

        let mut second_params = world.catalog_params(Some("production"));
        second_params.manifest_sha256 = Some(root.manifest_sha256);
        second_params.offset = 1;
        second_params.limit = 1;
        let second = world
            .service
            .catalog(second_params, &world.caller)
            .expect("second page");
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn file_returns_exact_base64_chunks_and_offsets() {
        let world = World::new();
        assert_eq!(base64_standard(b""), "");
        assert_eq!(base64_standard(b"f"), "Zg==");
        assert_eq!(base64_standard(b"fo"), "Zm8=");
        assert_eq!(base64_standard(b"foo"), "Zm9v");

        let relative = "nested/screenshot.png";
        let payload = world
            .files
            .iter()
            .find(|(name, _)| name == relative)
            .expect("fixture file")
            .1
            .clone();
        let first = world
            .service
            .file(world.file_params(relative, 7), &world.caller)
            .expect("first chunk");
        assert_eq!(first.base64, base64_standard(&payload[..7]));
        assert_eq!(first.bytes, 7);
        assert_eq!(first.offset, 0);
        assert_eq!(first.next_offset, Some(7));

        let mut second_params = world.file_params(relative, MAX_CHUNK_BYTES);
        second_params.offset = 7;
        let second = world
            .service
            .file(second_params, &world.caller)
            .expect("second chunk");
        assert_eq!(second.base64, base64_standard(&payload[7..]));
        assert_eq!(second.next_offset, None);
    }

    #[test]
    fn changed_symlinked_and_non_regular_files_are_tampered() {
        let world = World::new();
        let target = world.evidence.join("retained/production/report.json");
        fs::write(&target, b"same-size-bad\n").expect("tamper bytes");
        let error = world
            .service
            .catalog(world.catalog_params(Some("production")), &world.caller)
            .expect_err("changed bytes");
        assert_eq!(error.code, ErrorCode::TestArtifactTampered);
        assert!(
            !error
                .message
                .contains(&world.repository.to_string_lossy().into_owned())
        );

        fs::remove_file(&target).expect("remove changed file");
        symlink("nested/screenshot.png", &target).expect("alias file");
        let error = world
            .service
            .file(world.file_params("report.json", 10), &world.caller)
            .expect_err("symlink must not be followed");
        assert_eq!(error.code, ErrorCode::TestArtifactTampered);

        fs::remove_file(&target).expect("remove symlink");
        fs::create_dir(&target).expect("non-regular replacement");
        let error = world
            .service
            .file(world.file_params("report.json", 10), &world.caller)
            .expect_err("directory must not be read as a file");
        assert_eq!(error.code, ErrorCode::TestArtifactTampered);
    }

    #[test]
    fn manifest_identity_paging_and_relative_path_failures_keep_codes() {
        let world = World::new();
        let mut wrong_manifest = world.catalog_params(None);
        wrong_manifest.manifest_sha256 = Some("c".repeat(64));
        assert_eq!(
            world
                .service
                .catalog(wrong_manifest, &world.caller)
                .expect_err("manifest cursor")
                .code,
            ErrorCode::TestArtifactTampered
        );

        let mut past_end = world.catalog_params(Some("production"));
        past_end.offset = 3;
        assert_eq!(
            world
                .service
                .catalog(past_end, &world.caller)
                .expect_err("offset")
                .code,
            ErrorCode::ParamsInvalid
        );

        for relative in ["../secret", "/absolute", "nested//file", "nested\\file"] {
            let error = world
                .service
                .file(world.file_params(relative, 10), &world.caller)
                .expect_err("unsafe relative path");
            assert_eq!(error.code, ErrorCode::ParamsInvalid);
        }
    }

    #[test]
    fn missing_runs_checks_and_artifacts_have_distinct_stable_codes() {
        let world = World::new();
        let mut missing_run = world.catalog_params(None);
        missing_run.run_id = "t20260903T999999Z-absent".into();
        assert_eq!(
            world
                .service
                .catalog(missing_run, &world.caller)
                .expect_err("expired run")
                .code,
            ErrorCode::TestArtifactExpired
        );

        let mut missing_check = world.catalog_params(None);
        missing_check.check = "missing".into();
        assert_eq!(
            world
                .service
                .catalog(missing_check, &world.caller)
                .expect_err("missing check")
                .code,
            ErrorCode::TestArtifactNotFound
        );

        let missing_artifact = world.catalog_params(Some("developer-test"));
        assert_eq!(
            world
                .service
                .catalog(missing_artifact, &world.caller)
                .expect_err("missing artifact")
                .code,
            ErrorCode::TestArtifactNotFound
        );
    }

    #[test]
    fn malformed_manifest_and_run_metadata_are_tampered() {
        let world = World::new();
        let manifest_path = world.evidence.join(MANIFEST_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
                .expect("manifest JSON");
        manifest["unexpected"] = serde_json::Value::Bool(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("changed manifest"),
        )
        .expect("write changed manifest");
        assert_eq!(
            world
                .service
                .catalog(world.catalog_params(None), &world.caller)
                .expect_err("unknown manifest field")
                .code,
            ErrorCode::TestArtifactTampered
        );

        let world = World::new();
        let manifest_path = world.evidence.join(MANIFEST_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest"))
                .expect("manifest JSON");
        manifest["artifacts"][0]["sha256"] = serde_json::Value::String("c".repeat(64));
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("changed manifest"),
        )
        .expect("write changed manifest");
        assert_eq!(
            world
                .service
                .catalog(world.catalog_params(None), &world.caller)
                .expect_err("tree digest mismatch")
                .code,
            ErrorCode::TestArtifactTampered
        );

        let world = World::new();
        let manifest_path = world.evidence.join(MANIFEST_NAME);
        let outside = world.repository.join("outside-manifest.json");
        fs::rename(&manifest_path, &outside).expect("move manifest");
        symlink(&outside, &manifest_path).expect("manifest symlink");
        assert_eq!(
            world
                .service
                .catalog(world.catalog_params(None), &world.caller)
                .expect_err("manifest symlink")
                .code,
            ErrorCode::TestArtifactNotFound
        );

        let world = World::new();
        let run_path = world
            .repository
            .join(".devcoordinator/test/logs/runs")
            .join(RUN_ID)
            .join("run.json");
        let mut run: serde_json::Value =
            serde_json::from_slice(&fs::read(&run_path).expect("run")).expect("run JSON");
        run["complete"] = serde_json::Value::Bool(false);
        fs::write(&run_path, serde_json::to_vec(&run).expect("run JSON")).expect("write run");
        assert_eq!(
            world
                .service
                .catalog(world.catalog_params(None), &world.caller)
                .expect_err("inconsistent run")
                .code,
            ErrorCode::TestArtifactTampered
        );
    }

    #[test]
    fn public_callers_resolve_only_the_exact_registered_worktree() {
        let world = World::new();
        let mut nested = world.catalog_params(None);
        nested.path = world
            .repository
            .join("nested")
            .to_string_lossy()
            .into_owned();
        let error = world
            .service
            .catalog(nested, &world.caller)
            .expect_err("nested path is not an exact registration");
        assert_eq!(error.code, ErrorCode::RepositoryNotFound);
        assert!(
            !error
                .message
                .contains(&world.repository.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn cloned_service_shares_safe_verification_cache() {
        let world = World::new();
        let clone = world.service.clone();
        clone
            .catalog(world.catalog_params(Some("production")), &world.caller)
            .expect("verified through clone");
        assert!(
            !world
                .service
                .verified
                .lock()
                .expect("verification cache")
                .is_empty()
        );
        drop(world.database.clone());
    }
}
