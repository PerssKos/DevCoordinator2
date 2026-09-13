//! Retained formal-UI journey evidence and screenshot-anchored feedback.
//!
//! Evidence remains caller-owned cold data beneath an exact governed run log
//! leaf. Every component is opened relative to a held directory descriptor
//! with `O_NOFOLLOW`; public results contain stable identities rather than
//! host paths. Screenshot feedback is an immutable overlay linked atomically
//! to an ordinary Plan `user_feedback` task.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use devcoordinator2_api::params::{
    CreateFeedback, EvidenceImage, EvidenceReference, FeedbackDelete, FeedbackEdit, FeedbackReply,
    FeedbackState, FeedbackStateChange, Mark, Point, TaskStatus,
};
use devcoordinator2_api::results::{
    AvailableScreenshot, EarlierVisualEvidence, EvidenceAction, EvidenceBundle, EvidenceCell,
    EvidenceCoverage, EvidenceFinding, EvidenceGet, EvidenceIssue, EvidenceRequiredCoverage,
    EvidenceReview, EvidenceScreenshots, EvidenceViewport, Feedback, FeedbackComment,
    FeedbackCreated, FeedbackMutation, ImageChunk, Screenshot, TestList, UnavailableScreenshot,
    VisualEvidenceSummary,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use regex::Regex;
use rusqlite::OptionalExtension;
use rustix::fs::{self as unix_fs, Dir, Mode, OFlags};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::{format_description::FormatItem, macros::format_description};

use crate::access::Caller;
use crate::database::{Database, DatabaseError};
use crate::ids;
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;

const MANIFEST_NAME: &str = "journey-evidence.json";
const MULTI_BUNDLE_DIRECTORY: &str = "formal-runs";
const MANIFEST_KIND: &str = "formal-web-ui-journey-evidence";
const MANIFEST_SCHEMA: u8 = 1;
const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_CHUNK_BYTES: u32 = 180 * 1024;
const MAX_MANIFESTS: usize = 256;
const MAX_CELLS: usize = 512;
const MAX_COMMENTS: usize = 512;
const MAX_MARKS: usize = 64;
const MAX_POINTS: usize = 256;
const HASH_BLOCK_BYTES: usize = 1024 * 1024;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
const COLORS: &[&str] = &[
    "#4c8dff", "#f59e0b", "#ef4444", "#22c55e", "#a855f7", "#f8fafc",
];

#[derive(Clone)]
pub struct TestEvidenceService {
    database: Database,
    registry: Registry,
    clock: Arc<dyn Clock>,
}

impl TestEvidenceService {
    pub fn new(database: Database, registry: Registry) -> Self {
        Self::with_clock(database, registry, Arc::new(HostClock))
    }

    pub fn with_clock(database: Database, registry: Registry, clock: Arc<dyn Clock>) -> Self {
        Self {
            database,
            registry,
            clock,
        }
    }

    pub fn get(
        &self,
        params: EvidenceReference,
        caller: &Caller,
    ) -> Result<EvidenceGet, ProtocolError> {
        validate_run_id(&params.run_id)?;
        let resolved = self.resolve(Path::new(&params.path), caller)?;
        let loaded = self.load(&resolved.worktree, &params.run_id)?;
        let feedback = self.feedback_for_run(
            &resolved.repository_id,
            &resolved.worktree_id,
            &params.run_id,
            &caller.actor(),
        )?;
        Ok(EvidenceGet {
            repository_id: resolved.repository_id,
            worktree_id: resolved.worktree_id,
            run_id: params.run_id,
            status: availability(&loaded.bundles),
            bundles: loaded.bundles,
            feedback,
            issues_truncated: loaded.issues.len() >= 64,
            image_count: bounded_u32(loaded.images.len()),
            issues: loaded.issues,
        })
    }

    pub fn summary_registered(
        &self,
        worktree: &Path,
        run_id: &str,
    ) -> Result<VisualEvidenceSummary, ProtocolError> {
        validate_run_id(run_id)?;
        let loaded = self.load(worktree, run_id)?;
        Ok(VisualEvidenceSummary {
            status: availability(&loaded.bundles),
            bundle_count: bounded_u32(loaded.bundles.len()),
            image_count: bounded_u32(loaded.images.len()),
            issue_count: bounded_u32(loaded.issues.len()),
            issues_truncated: loaded.issues.len() >= 64,
            error_code: None,
        })
    }

    pub fn lookup(
        &self,
        params: devcoordinator2_api::params::EvidenceLookup,
        caller: &Caller,
    ) -> Result<devcoordinator2_api::results::EvidenceLookup, ProtocolError> {
        validate_run_id(&params.run_id)?;
        if let Some(image_id) = &params.image_id {
            validate_digest(image_id, "image_id")?;
        }
        let worktrees = self.database.call(|connection| {
            let mut statement = connection.prepare("SELECT w.worktree_id,w.worktree_path,w.repository_id,r.display_name FROM worktrees w JOIN repositories r ON r.repository_id=w.repository_id ORDER BY w.worktree_id")?;
            Ok(statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)))?.collect::<Result<Vec<_>, _>>()?)
        }).map_err(database_error)?;
        let mut found = None;
        for (worktree_id, worktree_path, repository_id, display_name) in worktrees {
            if params
                .worktree_id
                .as_ref()
                .is_some_and(|requested| requested != &worktree_id)
            {
                continue;
            }
            let worktree = Path::new(&worktree_path);
            let Ok(loaded) = self.load(worktree, &params.run_id) else {
                continue;
            };
            if loaded.images.is_empty()
                || params
                    .image_id
                    .as_ref()
                    .is_some_and(|image_id| !loaded.images.contains_key(image_id))
            {
                continue;
            }
            if found.is_some() {
                return Err(invalid_argument(
                    "More than one registered worktree contains this screenshot. Open the run from its repository in Tests.",
                ));
            }
            let history = crate::test_state::TestRunStore
                .read_history(worktree)
                .unwrap_or_default();
            let previous = history
                .into_iter()
                .find(|entry| entry.run_id == params.run_id);
            let current = crate::test_state::TestRunStore
                .read_current_summary(worktree)
                .ok()
                .flatten()
                .filter(|entry| entry.run_id == params.run_id);
            let context = devcoordinator2_api::results::EvidenceRunContext {
                repository_id: repository_id.clone(),
                worktree_id: worktree_id.clone(),
                worktree_path,
                display_name,
                run_id: params.run_id.clone(),
                test: previous
                    .as_ref()
                    .map(|entry| entry.test.clone())
                    .or_else(|| current.as_ref().map(|entry| entry.test.clone())),
                started_at: previous
                    .as_ref()
                    .map(|entry| entry.started_at.clone())
                    .or_else(|| current.as_ref().map(|entry| entry.started_at.clone())),
            };
            let feedback = self.feedback_for_run(
                &repository_id,
                &worktree_id,
                &params.run_id,
                &caller.actor(),
            )?;
            let evidence = EvidenceGet {
                repository_id,
                worktree_id,
                run_id: params.run_id.clone(),
                status: availability(&loaded.bundles),
                image_count: bounded_u32(loaded.images.len()),
                issues_truncated: loaded.issues.len() >= 64,
                bundles: loaded.bundles,
                feedback,
                issues: loaded.issues,
            };
            found = Some(devcoordinator2_api::results::EvidenceLookup { context, evidence });
        }
        found.ok_or_else(expired)
    }

    /// Attach bounded evidence availability to lifecycle-owned current runs
    /// without repeating caller Git discovery for every row.
    pub fn enrich_list(&self, list: &mut TestList) {
        for run in &mut list.runs {
            run.visual_evidence = self
                .summary_registered(Path::new(&run.worktree_path), &run.summary.run_id)
                .unwrap_or_else(|error| VisualEvidenceSummary {
                    status: "unavailable".to_owned(),
                    bundle_count: 0,
                    image_count: 0,
                    issue_count: 0,
                    issues_truncated: false,
                    error_code: Some(error.code.to_string()),
                });
            run.earlier_visual_evidence = crate::test_state::TestRunStore
                .read_history(Path::new(&run.worktree_path))
                .unwrap_or_default()
                .into_iter()
                .rev()
                .filter(|entry| entry.run_id != run.summary.run_id)
                .find_map(|entry| {
                    let evidence = self
                        .summary_registered(Path::new(&run.worktree_path), &entry.run_id)
                        .ok()?;
                    (evidence.image_count > 0).then_some(EarlierVisualEvidence {
                        run_id: entry.run_id,
                        test: entry.test,
                        started_at: entry.started_at,
                        visual_evidence: evidence,
                    })
                });
        }
    }

    pub fn image(
        &self,
        params: EvidenceImage,
        caller: &Caller,
    ) -> Result<ImageChunk, ProtocolError> {
        validate_run_id(&params.run_id)?;
        validate_digest(&params.image_id, "image_id")?;
        if params.max_bytes == 0 || params.max_bytes > MAX_IMAGE_CHUNK_BYTES {
            return Err(invalid_argument("'max_bytes' must be positive"));
        }
        let resolved = self.resolve(Path::new(&params.path), caller)?;
        let loaded = self.load(&resolved.worktree, &params.run_id)?;
        let image = loaded
            .images
            .get(&params.image_id)
            .ok_or_else(evidence_not_found)?;
        let offset = u64::from(params.offset);
        if offset > image.size {
            return Err(invalid_argument("'offset' is beyond the screenshot"));
        }
        let (file, before) = verified_image_file(&resolved.worktree, &params.run_id, image)?;
        let wanted = usize::try_from((image.size - offset).min(u64::from(params.max_bytes)))
            .expect("image chunk is bounded to usize");
        let block = read_exact_at(&file, offset, wanted)
            .map_err(|_| tampered("The screenshot changed while it was read."))?;
        let after = file
            .metadata()
            .map_err(|_| tampered("The screenshot changed while it was read."))?;
        if FileIdentity::from_metadata(&after) != before {
            return Err(tampered("The screenshot changed while it was read."));
        }
        let bytes = bounded_u32(block.len());
        let next = offset + u64::from(bytes);
        Ok(ImageChunk {
            image_id: image.image_id.clone(),
            mime: image.mime.clone(),
            sha256: image.sha256.clone(),
            total_bytes: image.size,
            offset,
            bytes,
            base64: BASE64.encode(block),
            next_offset: (next < image.size).then_some(next),
        })
    }

    fn load(&self, worktree: &Path, run_id: &str) -> Result<LoadedEvidence, ProtocolError> {
        let run = open_run_directory(worktree, run_id).map_err(|_| expired())?;
        let mut bundles = Vec::new();
        let mut images = HashMap::new();
        let mut issues = Vec::new();
        for leaf in manifest_leaves(&run) {
            let Ok(evidence) = open_evidence_directory(&run, &leaf) else {
                continue;
            };
            let remaining = MAX_MANIFESTS.saturating_sub(bundles.len());
            for directory in manifest_directories(&evidence, remaining) {
                let result = (|| {
                    let manifest_directory = open_relative_directory(&evidence, &directory)?;
                    let raw = read_bounded_regular(
                        &manifest_directory,
                        MANIFEST_NAME,
                        MAX_MANIFEST_BYTES,
                    )?;
                    sanitize_manifest(&manifest_directory, &raw, &leaf, run_id, &directory)
                })();
                match result {
                    Ok((bundle, bundle_images)) => {
                        bundles.push(bundle);
                        images.extend(bundle_images);
                    }
                    Err(_) if issues.len() < 64 => issues.push(EvidenceIssue {
                        check: leaf.check.clone(),
                        phase: leaf.phase.clone(),
                        case: leaf.case.clone().unwrap_or_default(),
                        code: "invalid_evidence".to_owned(),
                    }),
                    Err(_) => {}
                }
            }
            if bundles.len() >= MAX_MANIFESTS {
                break;
            }
        }
        bundles.sort_by(|left, right| {
            (
                &left.check,
                &left.phase,
                left.case.as_deref().unwrap_or(""),
                &left.formal_run_id,
            )
                .cmp(&(
                    &right.check,
                    &right.phase,
                    right.case.as_deref().unwrap_or(""),
                    &right.formal_run_id,
                ))
        });
        Ok(LoadedEvidence {
            bundles,
            images,
            issues,
        })
    }

    pub(crate) fn repository_id_for(
        &self,
        path: &Path,
        caller: &Caller,
    ) -> Result<String, ProtocolError> {
        Ok(self.resolve(path, caller)?.repository_id)
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct Leaf {
    check: String,
    phase: String,
    case: Option<String>,
}

#[derive(Clone, Debug)]
struct Image {
    image_id: String,
    leaf: Leaf,
    relative_path: String,
    mime: String,
    size: u64,
    sha256: String,
    width: u32,
    height: u32,
    kind: String,
    cell: EvidenceCell,
}

#[derive(Debug)]
struct LoadedEvidence {
    bundles: Vec<EvidenceBundle>,
    images: HashMap<String, Image>,
    issues: Vec<EvidenceIssue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadError {
    Unavailable,
    Unsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

fn open_absolute_directory(path: &Path) -> Result<File, ReadError> {
    if !path.is_absolute() {
        return Err(ReadError::Unsafe);
    }
    let mut directory = unix_fs::open(
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| ReadError::Unavailable)?;
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
                .map_err(|_| ReadError::Unavailable)?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(ReadError::Unsafe);
            }
        }
    }
    Ok(directory)
}

fn open_directory(parent: &File, name: &str) -> Result<File, ReadError> {
    if !safe_component(name) {
        return Err(ReadError::Unsafe);
    }
    unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| ReadError::Unavailable)
}

fn open_run_directory(worktree: &Path, run_id: &str) -> Result<File, ReadError> {
    let mut directory = open_absolute_directory(worktree)?;
    for component in [".devcoordinator", "test", "logs", "runs", run_id] {
        directory = open_directory(&directory, component)?;
    }
    Ok(directory)
}

fn open_evidence_directory(run: &File, leaf: &Leaf) -> Result<File, ReadError> {
    let checks = open_directory(run, "checks")?;
    let check = open_directory(&checks, &leaf.check)?;
    let phase = if leaf.phase == "case" {
        let cases = open_directory(&check, "cases")?;
        open_directory(&cases, leaf.case.as_deref().ok_or(ReadError::Unsafe)?)?
    } else {
        open_directory(&check, &leaf.phase)?
    };
    open_directory(&phase, "evidence")
}

fn open_relative_directory(parent: &File, components: &[String]) -> Result<File, ReadError> {
    let mut directory = parent.try_clone().map_err(|_| ReadError::Unavailable)?;
    for component in components {
        directory = open_directory(&directory, component)?;
    }
    Ok(directory)
}

fn open_relative_file(parent: &File, relative: &str) -> Result<File, ReadError> {
    validate_relative_path(relative)?;
    let mut components = relative.split('/').peekable();
    let mut directory = parent.try_clone().map_err(|_| ReadError::Unavailable)?;
    while let Some(component) = components.next() {
        if components.peek().is_some() {
            directory = open_directory(&directory, component)?;
        } else {
            return unix_fs::openat(
                &directory,
                component,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| ReadError::Unavailable);
        }
    }
    Err(ReadError::Unsafe)
}

fn read_bounded_regular(parent: &File, name: &str, maximum: usize) -> Result<Vec<u8>, ReadError> {
    if !safe_component(name) {
        return Err(ReadError::Unsafe);
    }
    let mut file = unix_fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| ReadError::Unavailable)?;
    let before_metadata = file.metadata().map_err(|_| ReadError::Unavailable)?;
    if !before_metadata.is_file() || before_metadata.len() > maximum as u64 {
        return Err(ReadError::Unsafe);
    }
    let before = FileIdentity::from_metadata(&before_metadata);
    let mut bytes = Vec::with_capacity(before.size as usize);
    file.by_ref()
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadError::Unavailable)?;
    let after = file.metadata().map_err(|_| ReadError::Unavailable)?;
    if FileIdentity::from_metadata(&after) != before
        || bytes.len() as u64 != before.size
        || bytes.len() > maximum
    {
        return Err(ReadError::Unsafe);
    }
    Ok(bytes)
}

fn manifest_leaves(run: &File) -> Vec<Leaf> {
    let Ok(checks_directory) = open_directory(run, "checks") else {
        return Vec::new();
    };
    let mut checks = directory_names(&checks_directory, check_regex);
    checks.truncate(64);
    let mut leaves = Vec::new();
    for check_name in checks {
        let Ok(check_directory) = open_directory(&checks_directory, &check_name) else {
            continue;
        };
        for phase in ["check", "discovery"] {
            let leaf = Leaf {
                check: check_name.clone(),
                phase: phase.to_owned(),
                case: None,
            };
            if open_evidence_directory(run, &leaf).is_ok() {
                leaves.push(leaf);
            }
        }
        let Ok(cases_directory) = open_directory(&check_directory, "cases") else {
            continue;
        };
        for case_name in directory_names(&cases_directory, case_regex)
            .into_iter()
            .take(MAX_MANIFESTS)
        {
            let leaf = Leaf {
                check: check_name.clone(),
                phase: "case".to_owned(),
                case: Some(case_name),
            };
            if open_evidence_directory(run, &leaf).is_ok() {
                leaves.push(leaf);
                if leaves.len() >= MAX_MANIFESTS {
                    return leaves;
                }
            }
        }
    }
    leaves.truncate(MAX_MANIFESTS);
    leaves
}

fn manifest_directories(evidence: &File, maximum: usize) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    if maximum == 0 {
        return result;
    }
    if has_regular_file(evidence, MANIFEST_NAME) {
        result.push(Vec::new());
    }
    if result.len() >= maximum {
        return result;
    }
    let Ok(runs) = open_directory(evidence, MULTI_BUNDLE_DIRECTORY) else {
        return result;
    };
    for name in directory_names(&runs, digest_regex) {
        if result.len() >= maximum {
            break;
        }
        let Ok(bundle) = open_directory(&runs, &name) else {
            continue;
        };
        if has_regular_file(&bundle, MANIFEST_NAME) {
            result.push(vec![MULTI_BUNDLE_DIRECTORY.to_owned(), name]);
        }
    }
    result
}

fn has_regular_file(directory: &File, name: &str) -> bool {
    unix_fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .ok()
    .and_then(|file| file.metadata().ok())
    .is_some_and(|metadata| metadata.is_file())
}

fn directory_names(directory: &File, predicate: fn() -> &'static Regex) -> Vec<String> {
    let Ok(mut entries) = Dir::read_from(directory) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in &mut entries {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if predicate().is_match(name) {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

fn validate_relative_path(value: &str) -> Result<(), ReadError> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value.split('/').any(|part| !safe_component(part))
    {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: u8,
    kind: String,
    run_id: String,
    governed_run_id: String,
    governed_check: String,
    generated_at: String,
    browser: String,
    coverage: ManifestCoverage,
    cells: Vec<ManifestCell>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCoverage {
    checked_pages: u32,
    planned_pages: u32,
    failed: bool,
    readiness_eligible: bool,
    required_coverage: Option<EvidenceRequiredCoverage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCell {
    cell_id: String,
    review_cell_key: Option<String>,
    plan_index: Option<u32>,
    target_name: String,
    primary_journey: Option<String>,
    state_name: String,
    requested_path: Option<String>,
    final_path: Option<String>,
    viewport: ManifestViewport,
    started_at: Option<String>,
    ended_at: Option<String>,
    duration_ms: Option<u64>,
    outcome: String,
    http_status: Option<u16>,
    source_binding_status: String,
    review: Option<ManifestReview>,
    actions: Vec<ManifestAction>,
    findings: Vec<ManifestFinding>,
    screenshots: ManifestScreenshots,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestViewport {
    name: String,
    width: u32,
    height: u32,
    device: Option<String>,
    #[allow(dead_code)]
    sampling: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestAction {
    index: u32,
    action: String,
    outcome: String,
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFinding {
    severity: String,
    rule: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestReview {
    status: String,
    decision: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestScreenshots {
    viewport: Option<ManifestScreenshot>,
    full_page: Option<ManifestScreenshot>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestScreenshot {
    kind: String,
    path: String,
    mime: String,
    size: u64,
    sha256: String,
    width: u32,
    height: u32,
    captured_at: Option<String>,
}

fn sanitize_manifest(
    manifest_directory: &File,
    raw: &[u8],
    leaf: &Leaf,
    governed_run_id: &str,
    directory: &[String],
) -> Result<(EvidenceBundle, HashMap<String, Image>), ReadError> {
    validate_manifest_shape(raw)?;
    let manifest: Manifest = serde_json::from_slice(raw).map_err(|_| ReadError::Unsafe)?;
    if manifest.schema_version != MANIFEST_SCHEMA
        || manifest.kind != MANIFEST_KIND
        || manifest.governed_run_id != governed_run_id
        || manifest.governed_check != leaf.check
        || manifest.coverage.checked_pages > manifest.coverage.planned_pages
        || manifest.coverage.planned_pages as usize > MAX_CELLS
        || manifest.cells.len() > MAX_CELLS
    {
        return Err(ReadError::Unsafe);
    }
    bounded_required(&manifest.run_id, 128)?;
    bounded_required(&manifest.generated_at, 64)?;
    bounded_required(&manifest.browser, 256)?;
    validate_required_coverage(&manifest.coverage)?;
    let manifest_sha = sha256_hex(raw);
    let mut seen_cells = HashSet::new();
    let mut cells = Vec::with_capacity(manifest.cells.len());
    let mut images = HashMap::new();
    for source in manifest.cells {
        if !seen_cells.insert(source.cell_id.clone()) {
            return Err(ReadError::Unsafe);
        }
        let mut cell = sanitize_cell(&source, &manifest.run_id)?;
        let (viewport, viewport_image) = sanitize_screenshot(
            manifest_directory,
            leaf,
            directory,
            &manifest_sha,
            governed_run_id,
            "viewport",
            source.screenshots.viewport.as_ref(),
        )?;
        let (full_page, full_page_image) = sanitize_screenshot(
            manifest_directory,
            leaf,
            directory,
            &manifest_sha,
            governed_run_id,
            "full_page",
            source.screenshots.full_page.as_ref(),
        )?;
        cell.screenshots = EvidenceScreenshots {
            viewport,
            full_page,
        };
        for candidate in [viewport_image, full_page_image].into_iter().flatten() {
            images.insert(
                candidate.image_id.clone(),
                Image {
                    image_id: candidate.image_id,
                    leaf: leaf.clone(),
                    relative_path: candidate.relative_path,
                    mime: "image/png".to_owned(),
                    size: candidate.size,
                    sha256: candidate.sha256,
                    width: candidate.width,
                    height: candidate.height,
                    kind: candidate.kind,
                    cell: cell.clone(),
                },
            );
        }
        cells.push(cell);
    }
    Ok((
        EvidenceBundle {
            formal_run_id: manifest.run_id,
            generated_at: manifest.generated_at,
            browser: manifest.browser,
            check: leaf.check.clone(),
            phase: leaf.phase.clone(),
            case: leaf.case.clone(),
            coverage: EvidenceCoverage {
                checked_pages: manifest.coverage.checked_pages,
                planned_pages: manifest.coverage.planned_pages,
                failed: manifest.coverage.failed,
                readiness_eligible: manifest.coverage.readiness_eligible,
                required_coverage: manifest.coverage.required_coverage,
            },
            cells,
        },
        images,
    ))
}

fn validate_manifest_shape(raw: &[u8]) -> Result<(), ReadError> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|_| ReadError::Unsafe)?;
    exact_keys(
        &value,
        &[
            "schemaVersion",
            "kind",
            "runId",
            "governedRunId",
            "governedCheck",
            "generatedAt",
            "browser",
            "coverage",
            "cells",
        ],
    )?;
    allowed_keys(
        value.get("coverage").ok_or(ReadError::Unsafe)?,
        &[
            "checkedPages",
            "plannedPages",
            "failed",
            "readinessEligible",
            "requiredCoverage",
        ],
    )?;
    if let Some(required) = value
        .pointer("/coverage/requiredCoverage")
        .filter(|value| !value.is_null())
    {
        exact_keys(
            required,
            &["declaredCount", "satisfiedCount", "failed", "entries"],
        )?;
        for entry in required
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .ok_or(ReadError::Unsafe)?
        {
            allowed_keys(
                entry,
                &[
                    "requirementId",
                    "target",
                    "state",
                    "viewport",
                    "width",
                    "status",
                    "matchingCellIds",
                    "reason",
                ],
            )?;
        }
    }
    let cells = value
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .ok_or(ReadError::Unsafe)?;
    for cell in cells {
        exact_keys(
            cell,
            &[
                "cellId",
                "reviewCellKey",
                "planIndex",
                "targetName",
                "primaryJourney",
                "stateName",
                "requestedPath",
                "finalPath",
                "viewport",
                "startedAt",
                "endedAt",
                "durationMs",
                "outcome",
                "httpStatus",
                "sourceBindingStatus",
                "review",
                "actions",
                "findings",
                "screenshots",
            ],
        )?;
        let viewport = cell.get("viewport").ok_or(ReadError::Unsafe)?;
        allowed_keys(viewport, &["name", "width", "height", "device", "sampling"])?;
        for required in ["name", "width", "height"] {
            if viewport.get(required).is_none() {
                return Err(ReadError::Unsafe);
            }
        }
        if let Some(review) = cell.get("review").filter(|value| !value.is_null()) {
            exact_keys(review, &["status", "decision"])?;
        }
        let actions = cell
            .get("actions")
            .and_then(serde_json::Value::as_array)
            .ok_or(ReadError::Unsafe)?;
        for action in actions {
            exact_keys(action, &["index", "action", "outcome", "durationMs"])?;
        }
        let findings = cell
            .get("findings")
            .and_then(serde_json::Value::as_array)
            .ok_or(ReadError::Unsafe)?;
        for finding in findings {
            exact_keys(finding, &["severity", "rule"])?;
        }
        let screenshots = cell.get("screenshots").ok_or(ReadError::Unsafe)?;
        exact_keys(screenshots, &["viewport", "fullPage"])?;
        for key in ["viewport", "fullPage"] {
            if let Some(screenshot) = screenshots.get(key).filter(|value| !value.is_null()) {
                exact_keys(
                    screenshot,
                    &[
                        "kind",
                        "path",
                        "mime",
                        "size",
                        "sha256",
                        "width",
                        "height",
                        "capturedAt",
                    ],
                )?;
            }
        }
    }
    Ok(())
}

fn exact_keys(value: &serde_json::Value, expected: &[&str]) -> Result<(), ReadError> {
    let object = value.as_object().ok_or(ReadError::Unsafe)?;
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

fn allowed_keys(value: &serde_json::Value, expected: &[&str]) -> Result<(), ReadError> {
    let object = value.as_object().ok_or(ReadError::Unsafe)?;
    if object
        .keys()
        .any(|key| !expected.iter().any(|expected| key == expected))
    {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

fn validate_required_coverage(coverage: &ManifestCoverage) -> Result<(), ReadError> {
    let Some(required) = &coverage.required_coverage else {
        return Ok(());
    };
    let satisfied = required
        .entries
        .iter()
        .filter(|entry| entry.status == "satisfied")
        .count();
    if required.entries.len() > MAX_CELLS
        || required.declared_count as usize != required.entries.len()
        || required.satisfied_count as usize != satisfied
        || required.failed != (satisfied != required.entries.len())
        || (required.failed && !coverage.failed)
    {
        return Err(ReadError::Unsafe);
    }
    let mut seen = HashSet::new();
    let mut identifiers = HashSet::new();
    for entry in &required.entries {
        if let Some(identifier) = &entry.requirement_id {
            bounded_required(identifier, 128)?;
            if !identifiers.insert(identifier) {
                return Err(ReadError::Unsafe);
            }
        }
        bounded_required(&entry.target, 512)?;
        bounded_required(&entry.state, 128)?;
        bounded_required(&entry.viewport, 128)?;
        bounded_optional(Some(&entry.reason), 2048)?;
        if let Some(width) = entry.width {
            positive(width, 32_768)?;
        }
        if !seen.insert((&entry.target, &entry.state, &entry.viewport, entry.width))
            || entry.matching_cell_ids.len() > MAX_CELLS
        {
            return Err(ReadError::Unsafe);
        }
        let mut matching = HashSet::new();
        for cell_id in &entry.matching_cell_ids {
            bounded_required(cell_id, 128)?;
            if !matching.insert(cell_id) {
                return Err(ReadError::Unsafe);
            }
        }
        let valid = match entry.status.as_str() {
            "satisfied" => entry.matching_cell_ids.len() == 1 && entry.reason.is_empty(),
            "missing" => entry.matching_cell_ids.is_empty() && !entry.reason.is_empty(),
            "ambiguous" => entry.matching_cell_ids.len() > 1 && !entry.reason.is_empty(),
            _ => false,
        };
        if !valid {
            return Err(ReadError::Unsafe);
        }
    }
    Ok(())
}

fn sanitize_cell(source: &ManifestCell, formal_run_id: &str) -> Result<EvidenceCell, ReadError> {
    bounded_required(&source.cell_id, 256)?;
    if !cell_regex().is_match(&source.cell_id) {
        return Err(ReadError::Unsafe);
    }
    bounded_optional(source.review_cell_key.as_deref(), 128)?;
    if source
        .review_cell_key
        .as_deref()
        .is_some_and(|value| !digest_regex().is_match(value))
    {
        return Err(ReadError::Unsafe);
    }
    if source
        .plan_index
        .is_some_and(|value| value as usize > MAX_CELLS)
    {
        return Err(ReadError::Unsafe);
    }
    bounded_required(&source.target_name, 512)?;
    bounded_optional(source.primary_journey.as_deref(), 128)?;
    bounded_required(&source.state_name, 128)?;
    bounded_optional(source.requested_path.as_deref(), 2048)?;
    bounded_optional(source.final_path.as_deref(), 2048)?;
    bounded_required(&source.viewport.name, 128)?;
    positive(source.viewport.width, 32_768)?;
    positive(source.viewport.height, 32_768)?;
    bounded_optional(source.viewport.device.as_deref(), 128)?;
    bounded_optional(source.started_at.as_deref(), 64)?;
    bounded_optional(source.ended_at.as_deref(), 64)?;
    if source.duration_ms.is_some_and(|value| value > 86_400_000) {
        return Err(ReadError::Unsafe);
    }
    bounded_required(&source.outcome, 64)?;
    if source
        .http_status
        .is_some_and(|value| !(100..=599).contains(&value))
    {
        return Err(ReadError::Unsafe);
    }
    bounded_required(&source.source_binding_status, 64)?;
    if source.actions.len() > 128 || source.findings.len() > 64 {
        return Err(ReadError::Unsafe);
    }
    let mut actions = Vec::with_capacity(source.actions.len());
    for action in &source.actions {
        if action.index > 1024 || action.duration_ms > 86_400_000 {
            return Err(ReadError::Unsafe);
        }
        bounded_required(&action.action, 32)?;
        bounded_required(&action.outcome, 64)?;
        actions.push(EvidenceAction {
            index: action.index,
            action: action.action.clone(),
            outcome: action.outcome.clone(),
            duration_ms: action.duration_ms,
        });
    }
    let mut findings = Vec::with_capacity(source.findings.len());
    for finding in &source.findings {
        if !matches!(finding.severity.as_str(), "info" | "warning" | "critical") {
            return Err(ReadError::Unsafe);
        }
        bounded_required(&finding.rule, 128)?;
        findings.push(EvidenceFinding {
            severity: finding.severity.clone(),
            rule: finding.rule.clone(),
        });
    }
    let review = source
        .review
        .as_ref()
        .map(|review| {
            bounded_required(&review.status, 64)?;
            bounded_optional(review.decision.as_deref(), 32)?;
            Ok(EvidenceReview {
                status: review.status.clone(),
                decision: review.decision.clone(),
            })
        })
        .transpose()?;
    Ok(EvidenceCell {
        cell_id: source.cell_id.clone(),
        review_cell_key: source.review_cell_key.clone(),
        plan_index: source.plan_index,
        target_name: source.target_name.clone(),
        primary_journey: source.primary_journey.clone(),
        state_name: source.state_name.clone(),
        requested_path: source.requested_path.clone(),
        final_path: source.final_path.clone(),
        viewport: EvidenceViewport {
            name: source.viewport.name.clone(),
            width: source.viewport.width,
            height: source.viewport.height,
            device: source.viewport.device.clone(),
        },
        started_at: source.started_at.clone(),
        ended_at: source.ended_at.clone(),
        duration_ms: source.duration_ms,
        outcome: source.outcome.clone(),
        http_status: source.http_status,
        source_binding_status: source.source_binding_status.clone(),
        review,
        actions,
        findings,
        screenshots: EvidenceScreenshots {
            viewport: None,
            full_page: None,
        },
        formal_run_id: formal_run_id.to_owned(),
    })
}

#[derive(Debug)]
struct ImageCandidate {
    image_id: String,
    relative_path: String,
    size: u64,
    sha256: String,
    width: u32,
    height: u32,
    kind: String,
}

#[allow(clippy::too_many_arguments)]
fn sanitize_screenshot(
    manifest_directory: &File,
    leaf: &Leaf,
    directory: &[String],
    manifest_sha: &str,
    governed_run_id: &str,
    key: &str,
    source: Option<&ManifestScreenshot>,
) -> Result<(Option<Screenshot>, Option<ImageCandidate>), ReadError> {
    let Some(source) = source else {
        return Ok((None, None));
    };
    let expected_kind = if key == "viewport" {
        "viewport"
    } else {
        "full-page"
    };
    if source.kind != expected_kind
        || source.mime != "image/png"
        || !digest_regex().is_match(&source.sha256)
        || source.size == 0
        || source.size > MAX_IMAGE_BYTES
    {
        return Err(ReadError::Unsafe);
    }
    positive(source.width, 32_768)?;
    positive(source.height, 262_144)?;
    bounded_optional(source.captured_at.as_deref(), 64)?;
    validate_relative_path(&source.path)?;
    let available = open_relative_file(manifest_directory, &source.path)
        .and_then(|file| {
            let metadata = file.metadata().map_err(|_| ReadError::Unavailable)?;
            let mut header = [0u8; 24];
            file.read_exact_at(&mut header, 0)
                .map_err(|_| ReadError::Unavailable)?;
            if !metadata.is_file()
                || metadata.len() != source.size
                || png_dimensions(&header) != Some((source.width, source.height))
            {
                return Err(ReadError::Unsafe);
            }
            Ok(())
        })
        .is_ok();
    if !available {
        return Ok((
            Some(Screenshot::Unavailable(UnavailableScreenshot {
                status: "unavailable".to_owned(),
                kind: expected_kind.to_owned(),
            })),
            None,
        ));
    }
    let relative_path = directory
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(source.path.as_str()))
        .collect::<Vec<_>>()
        .join("/");
    let image_id = sha256_hex(
        [
            governed_run_id,
            &leaf.check,
            &leaf.phase,
            leaf.case.as_deref().unwrap_or(""),
            manifest_sha,
            &relative_path,
            &source.sha256,
        ]
        .join("\0")
        .as_bytes(),
    );
    Ok((
        Some(Screenshot::Available(AvailableScreenshot {
            status: "available".to_owned(),
            kind: expected_kind.to_owned(),
            image_id: image_id.clone(),
            mime: "image/png".to_owned(),
            size: source.size,
            sha256: source.sha256.clone(),
            width: source.width,
            height: source.height,
            captured_at: source.captured_at.clone(),
        })),
        Some(ImageCandidate {
            image_id,
            relative_path,
            size: source.size,
            sha256: source.sha256.clone(),
            width: source.width,
            height: source.height,
            kind: expected_kind.to_owned(),
        }),
    ))
}

fn verified_image_file(
    worktree: &Path,
    run_id: &str,
    image: &Image,
) -> Result<(File, FileIdentity), ProtocolError> {
    let run = open_run_directory(worktree, run_id)
        .map_err(|_| tampered("The screenshot no longer matches its evidence."))?;
    let evidence = open_evidence_directory(&run, &image.leaf)
        .map_err(|_| tampered("The screenshot no longer matches its evidence."))?;
    let file = open_relative_file(&evidence, &image.relative_path)
        .map_err(|_| tampered("The screenshot no longer matches its evidence."))?;
    let metadata = file
        .metadata()
        .map_err(|_| tampered("The screenshot no longer matches its evidence."))?;
    let before = FileIdentity::from_metadata(&metadata);
    let mut header = [0u8; 24];
    file.read_exact_at(&mut header, 0)
        .map_err(|_| tampered("The screenshot no longer matches its evidence."))?;
    if !metadata.is_file()
        || metadata.len() != image.size
        || png_dimensions(&header) != Some((image.width, image.height))
        || hash_open_file(&file, image.size).ok().as_deref() != Some(image.sha256.as_str())
    {
        return Err(tampered("The screenshot no longer matches its evidence."));
    }
    let after = file
        .metadata()
        .map_err(|_| tampered("The screenshot changed while it was read."))?;
    if FileIdentity::from_metadata(&after) != before {
        return Err(tampered("The screenshot changed while it was read."));
    }
    Ok((file, before))
}

fn hash_open_file(file: &File, expected_size: u64) -> Result<String, ReadError> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; HASH_BLOCK_BYTES];
    let mut offset = 0u64;
    while offset < expected_size {
        let wanted = usize::try_from((expected_size - offset).min(HASH_BLOCK_BYTES as u64))
            .map_err(|_| ReadError::Unsafe)?;
        let count = file
            .read_at(&mut buffer[..wanted], offset)
            .map_err(|_| ReadError::Unavailable)?;
        if count == 0 {
            return Err(ReadError::Unsafe);
        }
        digest.update(&buffer[..count]);
        offset += count as u64;
    }
    Ok(lower_hex(&digest.finalize()))
}

fn read_exact_at(file: &File, offset: u64, wanted: usize) -> Result<Vec<u8>, ReadError> {
    let mut bytes = vec![0u8; wanted];
    let mut filled = 0usize;
    while filled < wanted {
        let count = file
            .read_at(&mut bytes[filled..], offset + filled as u64)
            .map_err(|_| ReadError::Unavailable)?;
        if count == 0 {
            return Err(ReadError::Unsafe);
        }
        filled += count;
    }
    Ok(bytes)
}

fn png_dimensions(header: &[u8]) -> Option<(u32, u32)> {
    if header.len() < 24 || !header.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    Some((
        u32::from_be_bytes(header[16..20].try_into().ok()?),
        u32::from_be_bytes(header[20..24].try_into().ok()?),
    ))
}

fn bounded_required(value: &str, maximum: usize) -> Result<(), ReadError> {
    if value.is_empty() || value.len() > maximum {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

fn bounded_optional(value: Option<&str>, maximum: usize) -> Result<(), ReadError> {
    if value.is_some_and(|value| value.len() > maximum) {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

fn positive(value: u32, maximum: u32) -> Result<(), ReadError> {
    if value == 0 || value > maximum {
        return Err(ReadError::Unsafe);
    }
    Ok(())
}

fn availability(bundles: &[EvidenceBundle]) -> String {
    if bundles.is_empty() {
        "unavailable"
    } else {
        "available"
    }
    .to_owned()
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

fn bounded_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn run_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("constant run regex")
    })
}

fn check_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").expect("check regex"))
}

fn case_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$").expect("case regex"))
}

fn cell_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$").expect("cell regex"))
}

fn mark_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$").expect("mark regex"))
}

fn digest_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[0-9a-f]{64}$").expect("digest regex"))
}

fn validate_run_id(value: &str) -> Result<(), ProtocolError> {
    if run_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument("'run_id' is invalid"))
    }
}

fn validate_digest(value: &str, label: &str) -> Result<(), ProtocolError> {
    if digest_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument(format!("'{label}' is invalid")))
    }
}

impl TestEvidenceService {
    pub fn create_feedback(
        &self,
        params: CreateFeedback,
        caller: &Caller,
    ) -> Result<FeedbackCreated, ProtocolError> {
        validate_run_id(&params.run_id)?;
        validate_digest(&params.image_id, "image_id")?;
        let body = comment_text(&params.body, "body", 2_000)?;
        let marks = validate_marks(params.marks)?;
        let resolved = self.resolve(Path::new(&params.path), caller)?;
        let loaded = self.load(&resolved.worktree, &params.run_id)?;
        let image = loaded
            .images
            .get(&params.image_id)
            .cloned()
            .ok_or_else(evidence_not_found)?;
        let _ = verified_image_file(&resolved.worktree, &params.run_id, &image)?;

        let actor = caller.actor();
        let now = self.timestamp()?;
        let task_id = ids::task_id().map_err(id_error)?;
        let feedback_id = ids::feedback_id().map_err(id_error)?;
        let comment_id = ids::comment_id().map_err(id_error)?;
        let title = task_title(&body);
        let outcome = task_outcome(&body);
        let journey = image
            .cell
            .primary_journey
            .as_deref()
            .unwrap_or("this journey");
        let impact = format!(
            "The tested {} screen in {} does not yet match the owner's expectation.",
            image.cell.state_name, journey
        );
        let verification = "Address the marked visual feedback, rerun the same UI journey, and publish a new retained screenshot that the owner can inspect.".to_owned();
        let technical_note = format!(
            "Visual feedback {feedback_id}; governed run {}; check {}; formal cell {}; image {}; screenshot {}. Refs DC2-2026-09-02-VISUAL-JOURNEY-EVIDENCE and DC2-2026-09-02-SCREENSHOT-FEEDBACK.",
            params.run_id, image.leaf.check, image.cell.cell_id, image.image_id, image.sha256,
        );
        let geometry = encode_marks(&marks)?;

        let insert = FeedbackInsert {
            task_id: task_id.clone(),
            feedback_id: feedback_id.clone(),
            comment_id,
            repository_id: resolved.repository_id,
            worktree_id: resolved.worktree_id,
            run_id: params.run_id,
            check: image.leaf.check,
            phase: image.leaf.phase,
            case_id: image.leaf.case,
            formal_run_id: image.cell.formal_run_id,
            cell_id: image.cell.cell_id,
            review_cell_key: image.cell.review_cell_key,
            screenshot_kind: image.kind,
            screenshot_sha256: image.sha256,
            image_id: image.image_id,
            geometry,
            title,
            outcome,
            impact,
            verification,
            technical_note,
            body,
            actor: actor.clone(),
            now,
        };
        let position = self
            .database
            .transaction(move |transaction| insert_feedback(transaction, insert))
            .map_err(database_error)?;
        Ok(FeedbackCreated {
            task_id,
            feedback_id: feedback_id.clone(),
            position,
            feedback: self.feedback_for_id(&feedback_id, &actor)?,
        })
    }

    pub fn reply(
        &self,
        params: FeedbackReply,
        caller: &Caller,
    ) -> Result<FeedbackMutation, ProtocolError> {
        let feedback = self.feedback_target(
            Path::new(&params.path),
            &params.run_id,
            &params.feedback_id,
            caller,
        )?;
        if feedback.deleted_at.is_some() {
            return Err(invalid_argument("The visual feedback was deleted."));
        }
        let body = comment_text(&params.body, "body", 2_000)?;
        let actor = caller.actor();
        let now = self.timestamp()?;
        let comment_id = ids::comment_id().map_err(id_error)?;
        let feedback_id = feedback.feedback_id.clone();
        let result_id = feedback_id.clone();
        self.database
            .transaction(move |transaction| {
                let seq: u32 = transaction.query_row(
                    "SELECT COALESCE(MAX(seq),0)+1 FROM visual_feedback_comments WHERE feedback_id=?1",
                    [&feedback_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "INSERT INTO visual_feedback_comments(comment_id,feedback_id,seq,body,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?5)",
                    rusqlite::params![comment_id, feedback_id, seq, body, now, actor],
                )?;
                transaction.execute(
                    "UPDATE visual_feedback SET updated_at=?1 WHERE feedback_id=?2",
                    rusqlite::params![now, feedback_id],
                )?;
                append_feedback_event(
                    transaction,
                    &feedback_id,
                    "replied",
                    Some(&comment_id),
                    None,
                    Some(&body),
                    &actor,
                    &now,
                )?;
                append_plan_event(
                    transaction,
                    &feedback.repository_id,
                    "task",
                    &feedback.task_id,
                    "visual_feedback_reply",
                    None,
                    Some(&comment_id),
                    &actor,
                    &now,
                    Some(&truncate_chars(&body, 500)),
                )?;
                Ok(())
            })
            .map_err(database_error)?;
        Ok(FeedbackMutation {
            feedback: self.feedback_for_id(&result_id, &caller.actor())?,
        })
    }

    pub fn edit(
        &self,
        params: FeedbackEdit,
        caller: &Caller,
    ) -> Result<FeedbackMutation, ProtocolError> {
        let feedback = self.feedback_target(
            Path::new(&params.path),
            &params.run_id,
            &params.feedback_id,
            caller,
        )?;
        validate_mark_id(&params.comment_id, "comment_id")?;
        let body = comment_text(&params.body, "body", 2_000)?;
        let feedback_id = feedback.feedback_id.clone();
        let comment_id = params.comment_id.clone();
        let comment = self
            .database
            .call({
                let feedback_id = feedback_id.clone();
                let comment_id = comment_id.clone();
                move |connection| {
                    connection
                        .query_row(
                            "SELECT body,created_by,deleted_at FROM visual_feedback_comments WHERE comment_id=?1 AND feedback_id=?2",
                            rusqlite::params![comment_id, feedback_id],
                            |row| {
                                Ok(CommentOwner {
                                    body: row.get(0)?,
                                    created_by: row.get(1)?,
                                    deleted_at: row.get(2)?,
                                })
                            },
                        )
                        .optional()
                        .map_err(DatabaseError::from)
                }
            })
            .map_err(database_error)?
            .ok_or_else(|| invalid_argument("The comment does not belong to this feedback."))?;
        let actor = caller.actor();
        if comment.created_by != actor || comment.deleted_at.is_some() {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "Only the comment author can edit it.",
            ));
        }
        let now = self.timestamp()?;
        let result_id = feedback_id.clone();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE visual_feedback_comments SET body=?1,updated_at=?2 WHERE comment_id=?3",
                    rusqlite::params![body, now, comment_id],
                )?;
                transaction.execute(
                    "UPDATE visual_feedback SET updated_at=?1 WHERE feedback_id=?2",
                    rusqlite::params![now, feedback_id],
                )?;
                append_feedback_event(
                    transaction,
                    &feedback_id,
                    "comment_edited",
                    Some(&comment_id),
                    Some(&comment.body),
                    Some(&body),
                    &actor,
                    &now,
                )?;
                if comment_id == feedback.root_comment_id {
                    transaction.execute(
                        "UPDATE tasks SET title=?1,outcome=?2,updated_at=?3 WHERE task_id=?4",
                        rusqlite::params![
                            task_title(&body),
                            task_outcome(&body),
                            now,
                            feedback.task_id
                        ],
                    )?;
                    append_plan_event(
                        transaction,
                        &feedback.repository_id,
                        "task",
                        &feedback.task_id,
                        "edited",
                        None,
                        Some("outcome,title"),
                        &actor,
                        &now,
                        Some("Updated from the linked screenshot discussion."),
                    )?;
                }
                Ok(())
            })
            .map_err(database_error)?;
        Ok(FeedbackMutation {
            feedback: self.feedback_for_id(&result_id, &caller.actor())?,
        })
    }

    pub fn set_state(
        &self,
        params: FeedbackStateChange,
        caller: &Caller,
    ) -> Result<FeedbackMutation, ProtocolError> {
        let feedback = self.feedback_target(
            Path::new(&params.path),
            &params.run_id,
            &params.feedback_id,
            caller,
        )?;
        if feedback.deleted_at.is_some() {
            return Err(invalid_argument("The visual feedback was deleted."));
        }
        let (target, event, state) = match params.state {
            FeedbackState::Resolved => ("done", "resolved", "resolved"),
            FeedbackState::Open => ("planned", "reopened", "open"),
        };
        let actor = caller.actor();
        let result_id = feedback.feedback_id.clone();
        if feedback.task_status != target {
            let now = self.timestamp()?;
            self.database
                .transaction(move |transaction| {
                    transaction.execute(
                        "UPDATE tasks SET status=?1,updated_at=?2 WHERE task_id=?3",
                        rusqlite::params![target, now, feedback.task_id],
                    )?;
                    append_plan_event(
                        transaction,
                        &feedback.repository_id,
                        "task",
                        &feedback.task_id,
                        "status",
                        Some(&feedback.task_status),
                        Some(target),
                        &actor,
                        &now,
                        Some("Updated from the linked screenshot discussion."),
                    )?;
                    transaction.execute(
                        "UPDATE visual_feedback SET updated_at=?1 WHERE feedback_id=?2",
                        rusqlite::params![now, feedback.feedback_id],
                    )?;
                    append_feedback_event(
                        transaction,
                        &feedback.feedback_id,
                        event,
                        None,
                        None,
                        Some(state),
                        &actor,
                        &now,
                    )?;
                    Ok(())
                })
                .map_err(database_error)?;
        }
        Ok(FeedbackMutation {
            feedback: self.feedback_for_id(&result_id, &caller.actor())?,
        })
    }

    pub fn delete(
        &self,
        params: FeedbackDelete,
        caller: &Caller,
    ) -> Result<FeedbackMutation, ProtocolError> {
        let feedback = self.feedback_target(
            Path::new(&params.path),
            &params.run_id,
            &params.feedback_id,
            caller,
        )?;
        let actor = caller.actor();
        if feedback.created_by != actor {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "Only the annotation author can delete it.",
            ));
        }
        let result_id = feedback.feedback_id.clone();
        if feedback.deleted_at.is_none() {
            let now = self.timestamp()?;
            self.database
                .transaction(move |transaction| {
                    transaction.execute(
                        "UPDATE visual_feedback SET deleted_at=?1,deleted_by=?2,updated_at=?1 WHERE feedback_id=?3",
                        rusqlite::params![now, actor, feedback.feedback_id],
                    )?;
                    if feedback.task_status != "dropped" {
                        transaction.execute(
                            "UPDATE tasks SET status='dropped',updated_at=?1 WHERE task_id=?2",
                            rusqlite::params![now, feedback.task_id],
                        )?;
                        append_plan_event(
                            transaction,
                            &feedback.repository_id,
                            "task",
                            &feedback.task_id,
                            "status",
                            Some(&feedback.task_status),
                            Some("dropped"),
                            &actor,
                            &now,
                            Some("The linked screenshot annotation was explicitly deleted."),
                        )?;
                    }
                    append_feedback_event(
                        transaction,
                        &feedback.feedback_id,
                        "deleted",
                        None,
                        None,
                        Some("deleted"),
                        &actor,
                        &now,
                    )?;
                    Ok(())
                })
                .map_err(database_error)?;
        }
        Ok(FeedbackMutation {
            feedback: self.feedback_for_id(&result_id, &caller.actor())?,
        })
    }

    fn feedback_target(
        &self,
        path: &Path,
        run_id: &str,
        feedback_id: &str,
        caller: &Caller,
    ) -> Result<FeedbackRow, ProtocolError> {
        validate_run_id(run_id)?;
        validate_mark_id(feedback_id, "feedback_id")?;
        let resolved = self.resolve(path, caller)?;
        let feedback_id = feedback_id.to_owned();
        let repository_id = resolved.repository_id;
        let worktree_id = resolved.worktree_id;
        let run_id = run_id.to_owned();
        self.database
            .call(move |connection| {
                query_feedback(
                    connection,
                    "WHERE vf.feedback_id=?1 AND vf.repository_id=?2 AND vf.worktree_id=?3 AND vf.run_id=?4",
                    rusqlite::params![feedback_id, repository_id, worktree_id, run_id],
                )
            })
            .map_err(database_error)?
            .into_iter()
            .next()
            .ok_or_else(|| invalid_argument("The feedback does not belong to this test run."))
    }

    fn feedback_for_run(
        &self,
        repository_id: &str,
        worktree_id: &str,
        run_id: &str,
        actor: &str,
    ) -> Result<Vec<Feedback>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        let worktree_id = worktree_id.to_owned();
        let run_id = run_id.to_owned();
        let actor = actor.to_owned();
        self.database
            .call(move |connection| {
                let rows = query_feedback(
                    connection,
                    "WHERE vf.repository_id=?1 AND vf.worktree_id=?2 AND vf.run_id=?3 ORDER BY vf.created_at,vf.feedback_id LIMIT 512",
                    rusqlite::params![repository_id, worktree_id, run_id],
                )?;
                rows.into_iter()
                    .map(|row| public_feedback(connection, row, &actor))
                    .collect()
            })
            .map_err(database_error)
    }

    fn feedback_for_id(&self, feedback_id: &str, actor: &str) -> Result<Feedback, ProtocolError> {
        let feedback_id = feedback_id.to_owned();
        let actor = actor.to_owned();
        self.database
            .call(move |connection| {
                let row = query_feedback(
                    connection,
                    "WHERE vf.feedback_id=?1",
                    rusqlite::params![feedback_id],
                )?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    DatabaseError::Domain(invalid_argument("Visual feedback was not found."))
                })?;
                public_feedback(connection, row, &actor)
            })
            .map_err(database_error)
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InternalError, "cannot format feedback timestamp")
                    .with_detail(error.to_string())
            })
    }
}

struct FeedbackInsert {
    task_id: String,
    feedback_id: String,
    comment_id: String,
    repository_id: String,
    worktree_id: String,
    run_id: String,
    check: String,
    phase: String,
    case_id: Option<String>,
    formal_run_id: String,
    cell_id: String,
    review_cell_key: Option<String>,
    screenshot_kind: String,
    screenshot_sha256: String,
    image_id: String,
    geometry: String,
    title: String,
    outcome: String,
    impact: String,
    verification: String,
    technical_note: String,
    body: String,
    actor: String,
    now: String,
}

fn insert_feedback(
    transaction: &rusqlite::Transaction<'_>,
    insert: FeedbackInsert,
) -> Result<u32, DatabaseError> {
    let seq: u32 = transaction.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM tasks WHERE repository_id=?1",
        [&insert.repository_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO tasks(task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,created_at,created_by,updated_at) VALUES(?1,?2,NULL,NULL,?3,0,?4,?5,?6,?7,?7,?8,'user_feedback','planned',NULL,?9,?10,?9)",
        rusqlite::params![
            insert.task_id,
            insert.repository_id,
            seq,
            insert.title,
            insert.outcome,
            insert.impact,
            insert.verification,
            insert.technical_note,
            insert.now,
            insert.actor,
        ],
    )?;
    let position = place_root_task(transaction, &insert.repository_id, &insert.task_id)?;
    append_plan_event(
        transaction,
        &insert.repository_id,
        "task",
        &insert.task_id,
        "created",
        None,
        Some("user_feedback"),
        &insert.actor,
        &insert.now,
        Some("Created from a marked visual test screenshot."),
    )?;
    transaction.execute(
        "INSERT INTO visual_feedback(feedback_id,task_id,repository_id,worktree_id,run_id,check_name,phase,case_id,formal_run_id,cell_id,review_cell_key,screenshot_kind,screenshot_sha256,image_id,geometry_json,root_comment_id,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?17)",
        rusqlite::params![
            insert.feedback_id,
            insert.task_id,
            insert.repository_id,
            insert.worktree_id,
            insert.run_id,
            insert.check,
            insert.phase,
            insert.case_id,
            insert.formal_run_id,
            insert.cell_id,
            insert.review_cell_key,
            insert.screenshot_kind,
            insert.screenshot_sha256,
            insert.image_id,
            insert.geometry,
            insert.comment_id,
            insert.now,
            insert.actor,
        ],
    )?;
    transaction.execute(
        "INSERT INTO visual_feedback_comments(comment_id,feedback_id,seq,body,created_at,created_by,updated_at) VALUES(?1,?2,1,?3,?4,?5,?4)",
        rusqlite::params![
            insert.comment_id,
            insert.feedback_id,
            insert.body,
            insert.now,
            insert.actor,
        ],
    )?;
    append_feedback_event(
        transaction,
        &insert.feedback_id,
        "created",
        Some(&insert.comment_id),
        None,
        Some(&insert.body),
        &insert.actor,
        &insert.now,
    )?;
    Ok(position)
}

fn place_root_task(
    transaction: &rusqlite::Transaction<'_>,
    repository_id: &str,
    task_id: &str,
) -> Result<u32, DatabaseError> {
    let mut statement = transaction.prepare(
        "SELECT task_id FROM tasks WHERE repository_id=?1 AND parent_task_id IS NULL AND release_id IS NULL AND status!='dropped' AND task_id!=?2 ORDER BY position,seq",
    )?;
    let mut siblings = statement
        .query_map(rusqlite::params![repository_id, task_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    siblings.push(task_id.to_owned());
    for (offset, sibling) in siblings.iter().enumerate() {
        transaction.execute(
            "UPDATE tasks SET position=?1 WHERE task_id=?2",
            rusqlite::params![offset as u32 + 1, sibling],
        )?;
    }
    Ok(bounded_u32(siblings.len()))
}

#[allow(clippy::too_many_arguments)]
fn append_plan_event(
    transaction: &rusqlite::Transaction<'_>,
    repository_id: &str,
    subject_kind: &str,
    subject_id: &str,
    event: &str,
    from_value: Option<&str>,
    to_value: Option<&str>,
    actor: &str,
    now: &str,
    note: Option<&str>,
) -> Result<(), DatabaseError> {
    transaction.execute(
        "INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,from_value,to_value,actor,at,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![repository_id, subject_kind, subject_id, event, from_value, to_value, actor, now, note],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_feedback_event(
    transaction: &rusqlite::Transaction<'_>,
    feedback_id: &str,
    event: &str,
    comment_id: Option<&str>,
    from_value: Option<&str>,
    to_value: Option<&str>,
    actor: &str,
    now: &str,
) -> Result<(), DatabaseError> {
    transaction.execute(
        "INSERT INTO visual_feedback_events(feedback_id,event,comment_id,from_value,to_value,actor,at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![feedback_id, event, comment_id, from_value, to_value, actor, now],
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
struct FeedbackRow {
    feedback_id: String,
    task_id: String,
    repository_id: String,
    run_id: String,
    check: String,
    phase: String,
    case_id: Option<String>,
    formal_run_id: String,
    cell_id: String,
    review_cell_key: Option<String>,
    screenshot_kind: String,
    screenshot_sha256: String,
    image_id: String,
    geometry_json: String,
    root_comment_id: String,
    created_at: String,
    created_by: String,
    updated_at: String,
    deleted_at: Option<String>,
    task_status: String,
}

fn query_feedback<P: rusqlite::Params>(
    connection: &rusqlite::Connection,
    suffix: &str,
    params: P,
) -> Result<Vec<FeedbackRow>, DatabaseError> {
    let sql = format!(
        "SELECT vf.feedback_id,vf.task_id,vf.repository_id,vf.run_id,vf.check_name,vf.phase,vf.case_id,vf.formal_run_id,vf.cell_id,vf.review_cell_key,vf.screenshot_kind,vf.screenshot_sha256,vf.image_id,vf.geometry_json,vf.root_comment_id,vf.created_at,vf.created_by,vf.updated_at,vf.deleted_at,t.status FROM visual_feedback vf JOIN tasks t ON t.task_id=vf.task_id {suffix}"
    );
    let mut statement = connection.prepare(&sql)?;
    Ok(statement
        .query_map(params, |row| {
            Ok(FeedbackRow {
                feedback_id: row.get(0)?,
                task_id: row.get(1)?,
                repository_id: row.get(2)?,
                run_id: row.get(3)?,
                check: row.get(4)?,
                phase: row.get(5)?,
                case_id: row.get(6)?,
                formal_run_id: row.get(7)?,
                cell_id: row.get(8)?,
                review_cell_key: row.get(9)?,
                screenshot_kind: row.get(10)?,
                screenshot_sha256: row.get(11)?,
                image_id: row.get(12)?,
                geometry_json: row.get(13)?,
                root_comment_id: row.get(14)?,
                created_at: row.get(15)?,
                created_by: row.get(16)?,
                updated_at: row.get(17)?,
                deleted_at: row.get(18)?,
                task_status: row.get(19)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn public_feedback(
    connection: &rusqlite::Connection,
    row: FeedbackRow,
    actor: &str,
) -> Result<Feedback, DatabaseError> {
    let mut statement = connection.prepare(
        "SELECT comment_id,body,created_at,created_by,updated_at,deleted_at FROM visual_feedback_comments WHERE feedback_id=?1 ORDER BY seq LIMIT ?2",
    )?;
    let comments = statement
        .query_map(
            rusqlite::params![row.feedback_id, MAX_COMMENTS as u32],
            |comment| {
                Ok(CommentRow {
                    comment_id: comment.get(0)?,
                    body: comment.get(1)?,
                    created_at: comment.get(2)?,
                    created_by: comment.get(3)?,
                    updated_at: comment.get(4)?,
                    deleted_at: comment.get(5)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let deleted = row.deleted_at.is_some();
    let task_status = parse_task_status(&row.task_status)?;
    let marks: Vec<Mark> = serde_json::from_str(&row.geometry_json).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(
                ErrorCode::InternalError,
                "stored annotation geometry is invalid",
            )
            .with_detail(error.to_string()),
        )
    })?;
    Ok(Feedback {
        feedback_id: row.feedback_id,
        task_id: row.task_id,
        task_status,
        state: if deleted {
            "deleted"
        } else if row.task_status == "done" {
            "resolved"
        } else {
            "open"
        }
        .to_owned(),
        run_id: row.run_id,
        check: row.check,
        phase: row.phase,
        case: row.case_id,
        formal_run_id: row.formal_run_id,
        cell_id: row.cell_id,
        review_cell_key: row.review_cell_key,
        image_id: row.image_id,
        screenshot_kind: row.screenshot_kind,
        screenshot_sha256: row.screenshot_sha256,
        marks,
        author: display_actor(&row.created_by),
        created_at: row.created_at,
        updated_at: row.updated_at,
        can_delete: !deleted && row.created_by == actor,
        comments_truncated: comments.len() >= MAX_COMMENTS,
        comments: comments
            .into_iter()
            .map(|comment| {
                let deleted = comment.deleted_at.is_some();
                FeedbackComment {
                    comment_id: comment.comment_id,
                    body: if deleted {
                        "Comment deleted".to_owned()
                    } else {
                        comment.body
                    },
                    author: display_actor(&comment.created_by),
                    created_at: comment.created_at,
                    updated_at: comment.updated_at,
                    deleted,
                    can_edit: !deleted && comment.created_by == actor,
                }
            })
            .collect(),
    })
}

struct CommentOwner {
    body: String,
    created_by: String,
    deleted_at: Option<String>,
}

struct CommentRow {
    comment_id: String,
    body: String,
    created_at: String,
    created_by: String,
    updated_at: String,
    deleted_at: Option<String>,
}

fn parse_task_status(value: &str) -> Result<TaskStatus, DatabaseError> {
    match value {
        "planned" => Ok(TaskStatus::Planned),
        "in_progress" => Ok(TaskStatus::InProgress),
        "done" => Ok(TaskStatus::Done),
        "dropped" => Ok(TaskStatus::Dropped),
        _ => Err(DatabaseError::Domain(ProtocolError::new(
            ErrorCode::InternalError,
            "stored task status is invalid",
        ))),
    }
}

fn validate_marks(marks: Vec<Mark>) -> Result<Vec<Mark>, ProtocolError> {
    if marks.is_empty() || marks.len() > MAX_MARKS {
        return Err(invalid_argument(format!(
            "'marks' must contain 1..{MAX_MARKS} annotations"
        )));
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(marks.len());
    for (index, mark) in marks.into_iter().enumerate() {
        let normalized_mark = match mark {
            Mark::Pin { id, color, x, y } => Mark::Pin {
                id: checked_mark_id(id, index, &mut seen)?,
                color: checked_color(color, index)?,
                x: coordinate(x, "x")?,
                y: coordinate(y, "y")?,
            },
            Mark::Text {
                id,
                color,
                x,
                y,
                text,
            } => Mark::Text {
                id: checked_mark_id(id, index, &mut seen)?,
                color: checked_color(color, index)?,
                x: coordinate(x, "x")?,
                y: coordinate(y, "y")?,
                text: bounded_text(&text, "text", 1, 120)?,
            },
            Mark::Rectangle {
                id,
                color,
                x,
                y,
                width,
                height,
            } => {
                let x = coordinate(x, "x")?;
                let y = coordinate(y, "y")?;
                let width = coordinate(width, "width")?;
                let height = coordinate(height, "height")?;
                if width <= 0.0 || height <= 0.0 || x + width > 1.000_001 || y + height > 1.000_001
                {
                    return Err(invalid_argument(
                        "rectangle must remain inside the screenshot",
                    ));
                }
                Mark::Rectangle {
                    id: checked_mark_id(id, index, &mut seen)?,
                    color: checked_color(color, index)?,
                    x,
                    y,
                    width,
                    height,
                }
            }
            Mark::Arrow {
                id,
                color,
                x1,
                y1,
                x2,
                y2,
            } => Mark::Arrow {
                id: checked_mark_id(id, index, &mut seen)?,
                color: checked_color(color, index)?,
                x1: coordinate(x1, "x1")?,
                y1: coordinate(y1, "y1")?,
                x2: coordinate(x2, "x2")?,
                y2: coordinate(y2, "y2")?,
            },
            Mark::Freehand { id, color, points } => Mark::Freehand {
                id: checked_mark_id(id, index, &mut seen)?,
                color: checked_color(color, index)?,
                points: checked_points(points, index)?,
            },
            Mark::Highlight { id, color, points } => Mark::Highlight {
                id: checked_mark_id(id, index, &mut seen)?,
                color: checked_color(color, index)?,
                points: checked_points(points, index)?,
            },
        };
        normalized.push(normalized_mark);
    }
    let encoded = encode_marks(&normalized)?;
    if encoded.len() > 64 * 1024 {
        return Err(invalid_argument("annotation geometry is too large"));
    }
    Ok(normalized)
}

fn encode_marks(marks: &[Mark]) -> Result<String, ProtocolError> {
    let value = serde_json::to_value(marks).map_err(|error| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "cannot encode annotation geometry",
        )
        .with_detail(error.to_string())
    })?;
    serde_json::to_string(&canonical_json(value)).map_err(|error| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "cannot encode annotation geometry",
        )
        .with_detail(error.to_string())
    })
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonical_json(value));
            }
            serde_json::Value::Object(sorted)
        }
        scalar => scalar,
    }
}

fn checked_mark_id(
    id: String,
    index: usize,
    seen: &mut HashSet<String>,
) -> Result<String, ProtocolError> {
    if !mark_regex().is_match(&id) || !seen.insert(id.clone()) {
        return Err(invalid_argument(format!("marks[{index}].id is invalid")));
    }
    Ok(id)
}

fn checked_color(color: String, index: usize) -> Result<String, ProtocolError> {
    let color = color.to_ascii_lowercase();
    if !COLORS.contains(&color.as_str()) {
        return Err(invalid_argument(format!("marks[{index}].color is invalid")));
    }
    Ok(color)
}

fn checked_points(points: Vec<Point>, index: usize) -> Result<Vec<Point>, ProtocolError> {
    if !(2..=MAX_POINTS).contains(&points.len()) {
        return Err(invalid_argument(format!(
            "marks[{index}].points is invalid"
        )));
    }
    points
        .into_iter()
        .map(|point| {
            Ok(Point {
                x: coordinate(point.x, "x")?,
                y: coordinate(point.y, "y")?,
            })
        })
        .collect()
}

fn coordinate(value: f64, label: &str) -> Result<f64, ProtocolError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(invalid_argument(format!(
            "'{label}' must be between 0 and 1"
        )));
    }
    Ok((value * 1_000_000.0).round_ties_even() / 1_000_000.0)
}

fn validate_mark_id(value: &str, label: &str) -> Result<(), ProtocolError> {
    if mark_regex().is_match(value) {
        Ok(())
    } else {
        Err(invalid_argument(format!("'{label}' is invalid")))
    }
}

fn comment_text(value: &str, label: &str, maximum: usize) -> Result<String, ProtocolError> {
    bounded_text(value, label, 3, maximum)
}

fn bounded_text(
    value: &str,
    label: &str,
    minimum: usize,
    maximum: usize,
) -> Result<String, ProtocolError> {
    let value = value.trim();
    let length = value.chars().count();
    if length < minimum || length > maximum {
        return Err(invalid_argument(format!(
            "'{label}' must be plain text of {minimum}..{maximum} characters"
        )));
    }
    Ok(value.to_owned())
}

fn task_title(body: &str) -> String {
    let mut first = body
        .lines()
        .next()
        .unwrap_or(body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if first.chars().count() > 108 {
        let shortened = truncate_chars(&first, 107);
        first = shortened
            .rsplit_once(' ')
            .map_or(shortened.clone(), |(prefix, _)| prefix.to_owned());
        first.push('…');
    }
    let title = format!("Review: {first}");
    if title.chars().count() <= 120 {
        title
    } else {
        format!("{}…", truncate_chars(&title, 119))
    }
}

fn task_outcome(body: &str) -> String {
    if body.chars().count() >= 10 {
        body.to_owned()
    } else {
        format!("Change requested: {body}")
    }
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn display_actor(value: &str) -> String {
    if value.contains('@') {
        value
    } else {
        "Local administrator"
    }
    .to_owned()
}

fn expired() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestEvidenceExpired,
        "The selected visual evidence is unavailable or has expired.",
    )
}

fn evidence_not_found() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestEvidenceNotFound,
        "The selected screenshot is unavailable.",
    )
}

fn tampered(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::TestEvidenceTampered, message)
}

fn invalid_argument(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "visual feedback storage failed")
            .with_detail(other.to_string()),
    }
}

fn id_error(error: ids::IdError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "cannot create feedback identifier",
    )
    .with_detail(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::OperationExecutor;
    use devcoordinator2_api::{
        ClientKind,
        params::Point,
        results::{TestListRow, TestStatus},
    };
    use devcoordinator2_executor_protocol::{ProofKind, ValidationTier};
    use serde_json::{Value, json};
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use tempfile::TempDir;
    use time::macros::datetime;

    const RUN_ID: &str = "t20260902T010203Z-abcdef";
    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk/x8AAusB9Y9Z4rUAAAAASUVORK5CYII=";

    struct World {
        database: Database,
        service: TestEvidenceService,
        repo: PathBuf,
        evidence: PathBuf,
        screenshot: PathBuf,
        manifest: Value,
        repository_id: String,
        worktree_id: String,
        _temporary: TempDir,
    }

    fn caller(identity: &str) -> Caller {
        Caller {
            pid: std::process::id(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: ClientKind::Edge,
            client_session: None,
            work: None,
            identity: Some(identity.to_owned()),
        }
    }

    fn world() -> World {
        let temporary = tempfile::tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let registry = Registry::new(database.clone());
        let repo = temporary.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let registration = registry
            .register(
                &repo,
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            )
            .unwrap();
        let service = TestEvidenceService::with_clock(
            database.clone(),
            registry,
            Arc::new(crate::platform::FixedClock(
                datetime!(2026-09-02 1:03:00 UTC),
            )),
        );
        let evidence = repo
            .join(".devcoordinator/test/logs/runs")
            .join(RUN_ID)
            .join("checks/formal-ui/check/evidence");
        let screenshots = evidence.join("screenshots");
        fs::create_dir_all(&screenshots).unwrap();
        let screenshot = screenshots.join("cell-1-desktop-viewport.png");
        let png = BASE64.decode(PNG_BASE64).unwrap();
        fs::write(&screenshot, &png).unwrap();
        let digest = sha256_hex(&png);
        let manifest = json!({
            "schemaVersion": 1,
            "kind": "formal-web-ui-journey-evidence",
            "runId": "formal-web-ui-example",
            "governedRunId": RUN_ID,
            "governedCheck": "formal-ui",
            "generatedAt": "2026-09-02T01:03:00.000Z",
            "browser": "playwright-managed-browser",
            "coverage": {
                "checkedPages": 1,
                "plannedPages": 1,
                "failed": false,
                "readinessEligible": true
            },
            "cells": [{
                "cellId": "cell-1",
                "reviewCellKey": "a".repeat(64),
                "planIndex": 0,
                "targetName": "Sign in [invalid password]",
                "primaryJourney": "sign-in",
                "stateName": "invalid-password",
                "requestedPath": "/sign-in",
                "finalPath": "/sign-in",
                "viewport": {
                    "name": "desktop",
                    "width": 1440,
                    "height": 900,
                    "sampling": {"mode": "sampled-only"}
                },
                "startedAt": "2026-09-02T01:02:58.000Z",
                "endedAt": "2026-09-02T01:03:00.000Z",
                "durationMs": 2000,
                "outcome": "checked",
                "httpStatus": 200,
                "sourceBindingStatus": "matched",
                "review": {"status": "review-required", "decision": null},
                "actions": [{
                    "index": 0,
                    "action": "fill",
                    "outcome": "completed",
                    "durationMs": 12
                }],
                "findings": [{"severity": "warning", "rule": "tiny-interactive-target"}],
                "screenshots": {
                    "viewport": {
                        "kind": "viewport",
                        "path": "screenshots/cell-1-desktop-viewport.png",
                        "mime": "image/png",
                        "size": png.len(),
                        "sha256": digest,
                        "width": 1,
                        "height": 1,
                        "capturedAt": "2026-09-02T01:03:00.000Z"
                    },
                    "fullPage": null
                }
            }]
        });
        write_manifest(&evidence, &manifest);
        World {
            database,
            service,
            repo,
            evidence,
            screenshot,
            manifest,
            repository_id: registration.repository_id,
            worktree_id: registration.worktree_id,
            _temporary: temporary,
        }
    }

    fn write_manifest(directory: &Path, manifest: &Value) {
        fs::create_dir_all(directory).unwrap();
        fs::write(
            directory.join(MANIFEST_NAME),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap();
    }

    fn evidence(world: &World) -> EvidenceGet {
        world
            .service
            .get(
                EvidenceReference {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                },
                &caller("owner@example.test"),
            )
            .unwrap()
    }

    fn image_id(result: &EvidenceGet) -> String {
        match result.bundles[0].cells[0]
            .screenshots
            .viewport
            .as_ref()
            .unwrap()
        {
            Screenshot::Available(image) => image.image_id.clone(),
            Screenshot::Unavailable(_) => panic!("fixture screenshot is unavailable"),
        }
    }

    #[test]
    fn public_dispatcher_preserves_feedback_lifecycle_without_edge_git_access() {
        let world = world();
        fs::rename(
            world.repo.join(".git"),
            world.repo.join(".git-unavailable-fixture"),
        )
        .unwrap();
        let edge_uid = rustix::process::getuid().as_raw() + 1;
        let mut owner = caller("owner@example.test");
        owner.uid = edge_uid;
        owner.gid = edge_uid;
        assert!(
            world
                .service
                .registry
                .repository_status(&world.repo, Some((edge_uid, edge_uid)))
                .is_err()
        );
        world.database.transaction(|connection| {
            connection.execute("INSERT INTO users(user_id,email,administrator,created_at,created_by) VALUES('u1111111111111111','owner@example.test',1,'now','fixture')", [])?;
            Ok(())
        }).unwrap();
        let root = world._temporary.path();
        let plane = crate::control_plane::ControlPlane::with_adapters(
            crate::config::Config {
                socket_path: root.join("daemon.sock"),
                state_dir: root.join("state"),
                unit_prefix: "devcoordinator2-evidence-fixture".into(),
                slice_name: "unused-fixture.slice".into(),
                client_group: "unused-fixture".into(),
                port_range: (40000, 40100),
                base_domain: "example.test".into(),
                edge_uid: Some(edge_uid),
                admin_emails: Vec::new(),
                telegram_token_file: None,
                telegram_api: "https://api.telegram.org".into(),
                bugs_dir: root.join("bugs"),
                compose_env_allowlist_file: None,
                compose_env_authorizations: HashSet::new(),
                codex_usage_sources_file: None,
                codex_usage_sources: Vec::new(),
            },
            world.database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-08 17:00 UTC))),
        )
        .unwrap();
        let reference = json!({"path":world.repo,"run_id":RUN_ID});
        let readable = plane
            .execute("test.evidence.get", reference.clone(), &owner)
            .unwrap();
        assert_eq!(readable["image_count"], 1);
        let lookup = plane
            .execute(
                "test.evidence.lookup",
                json!({"run_id":RUN_ID,"image_id":image_id(&evidence(&world))}),
                &owner,
            )
            .unwrap();
        assert_eq!(lookup["context"]["worktree_id"], world.worktree_id);
        assert_eq!(lookup["context"]["repository_id"], world.repository_id);
        assert_eq!(lookup["evidence"]["image_count"], 1);
        assert!(
            plane
                .execute(
                    "test.evidence.lookup",
                    json!({"run_id":"../invalid"}),
                    &owner
                )
                .is_err()
        );
        assert!(
            plane
                .execute(
                    "test.evidence.lookup",
                    json!({"run_id":RUN_ID,"image_id":"f".repeat(64)}),
                    &owner
                )
                .is_err()
        );
        assert!(
            plane
                .execute(
                    "test.evidence.lookup",
                    json!({"run_id":"unretained-run"}),
                    &owner
                )
                .is_err()
        );
        let mut create = reference.clone();
        create["image_id"] = json!(image_id(&evidence(&world)));
        create["body"] = json!("Align the visible port and line");
        create["marks"] = json!([
            {"id":"pin-1","type":"pin","color":"#f59e0b","x":0.81,"y":0.24},
            {"id":"rectangle-1","type":"rectangle","color":"#f59e0b","x":0.76,"y":0.18,"width":0.20,"height":0.24}
        ]);
        let mut unregistered = create.clone();
        unregistered["path"] = json!(world.repo.join("unregistered"));
        assert_eq!(
            plane
                .execute("test.evidence.feedback.create", unregistered, &owner)
                .unwrap_err()
                .code,
            ErrorCode::RepositoryNotFound
        );
        let mut outsider = owner.clone();
        outsider.identity = Some("uninvited@example.test".into());
        assert!(
            plane
                .execute("test.evidence.lookup", json!({"run_id":RUN_ID}), &outsider)
                .is_err()
        );
        assert!(
            plane
                .execute("test.evidence.feedback.create", create.clone(), &outsider)
                .is_err()
        );
        let created = plane
            .execute("test.evidence.feedback.create", create, &owner)
            .unwrap();
        let task_id = created["task_id"].as_str().unwrap();
        let overview = plane
            .execute(
                "plan.overview",
                json!({"repository_id":world.repository_id}),
                &owner,
            )
            .unwrap();
        assert!(
            overview["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["task_id"] == task_id)
        );
        let readback = plane
            .execute("test.evidence.get", reference.clone(), &owner)
            .unwrap();
        assert_eq!(readback["feedback"].as_array().unwrap().len(), 1);
        assert_eq!(
            readback["feedback"][0]["marks"].as_array().unwrap().len(),
            2
        );
        let mut target = reference;
        target["feedback_id"] = created["feedback_id"].clone();
        let mut reply = target.clone();
        reply["body"] = json!("Retain this exact screenshot context");
        let replied = plane
            .execute("test.evidence.feedback.reply", reply, &owner)
            .unwrap();
        assert_eq!(replied["feedback"]["comments"].as_array().unwrap().len(), 2);
        let mut edit = target.clone();
        edit["comment_id"] = created["feedback"]["comments"][0]["comment_id"].clone();
        edit["body"] = json!("Align the port with the visible line");
        assert!(
            plane
                .execute("test.evidence.feedback.edit", edit, &owner)
                .is_ok()
        );
        for state in ["resolved", "open"] {
            let mut change = target.clone();
            change["state"] = json!(state);
            assert_eq!(
                plane
                    .execute("test.evidence.feedback.state", change, &owner)
                    .unwrap()["feedback"]["state"],
                state
            );
        }
        let deleted = plane
            .execute("test.evidence.feedback.delete", target, &owner)
            .unwrap();
        assert_eq!(deleted["feedback"]["state"], "deleted");
        world
            .database
            .call(move |connection| {
                let count: u32 = connection.query_row(
                    "SELECT COUNT(*) FROM tasks WHERE repository_id=?1 AND kind='user_feedback'",
                    [&world.repository_id],
                    |row| row.get(0),
                )?;
                assert_eq!(count, 1);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn retained_lookup_does_not_guess_between_copied_worktrees() {
        let world = world();
        let copied = world._temporary.path().join("copied-worktree");
        let evidence_path = copied
            .join(".devcoordinator/test/logs/runs")
            .join(RUN_ID)
            .join("checks/formal-ui/check/evidence");
        fs::create_dir_all(evidence_path.join("screenshots")).unwrap();
        fs::copy(
            &world.screenshot,
            evidence_path.join("screenshots/cell-1-desktop-viewport.png"),
        )
        .unwrap();
        write_manifest(&evidence_path, &world.manifest);
        let repository_id = world.repository_id.clone();
        world
            .database
            .transaction(move |connection| {
                connection.execute(
                    "INSERT INTO worktrees VALUES('w2222222222222222',?1,?2,'now','now')",
                    rusqlite::params![repository_id, copied.display().to_string()],
                )?;
                Ok(())
            })
            .unwrap();
        let error = world
            .service
            .lookup(
                devcoordinator2_api::params::EvidenceLookup {
                    run_id: RUN_ID.into(),
                    image_id: Some(image_id(&evidence(&world))),
                    worktree_id: None,
                },
                &caller("owner@example.test"),
            )
            .unwrap_err();
        assert!(error.message.contains("More than one registered worktree"));
        let exact = world
            .service
            .lookup(
                devcoordinator2_api::params::EvidenceLookup {
                    run_id: RUN_ID.into(),
                    image_id: Some(image_id(&evidence(&world))),
                    worktree_id: Some(world.worktree_id.clone()),
                },
                &caller("owner@example.test"),
            )
            .unwrap();
        assert_eq!(exact.context.worktree_id, world.worktree_id);
    }

    #[test]
    fn actual_verifier_producer_output_is_readable_and_images_remain_retrievable() {
        let world = world();
        let producer = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../skills/formal-web-ui-verification/scripts/formal_web_ui_verify.mjs");
        let source = r#"
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
const [producer,manifestPath]=process.argv.slice(2);
const {evaluateRequiredCoverage,normalizeRequiredCoverage,writeJourneyEvidenceArtifact}=await import(pathToFileURL(producer));
const original=JSON.parse(fs.readFileSync(manifestPath,'utf8'));
const pages=original.cells.map(cell=>({...cell,
  target:{name:cell.targetName,baseTargetName:cell.targetName,primaryJourney:cell.primaryJourney,stateName:cell.stateName},
  execution:{planIndex:cell.planIndex},review:{reviewCellKey:cell.reviewCellKey},
  status:cell.httpStatus,sourceBinding:{status:cell.sourceBindingStatus},actionTimings:cell.actions,
  screenshots:Object.fromEntries(Object.entries(cell.screenshots).map(([kind,image])=>[kind,image?{...image,path:path.join(path.dirname(manifestPath),image.path)}:null]))
}));
const requirements=normalizeRequiredCoverage(pages.map(cell=>({target:cell.target.baseTargetName,state:cell.target.stateName,viewport:cell.viewport.name})));
const coverage={...original.coverage,requiredCoverage:evaluateRequiredCoverage(pages,requirements)};
writeJourneyEvidenceArtifact({...original,pages,coverage,plan:{plannedPageCount:pages.length}},manifestPath);
"#;
        let result = Command::new("node")
            .args(["--input-type=module", "-e", source, "fixture-driver"])
            .arg(producer)
            .arg(world.evidence.join(MANIFEST_NAME))
            .env("DEVCOORDINATOR_RUN_ID", RUN_ID)
            .env("DEVCOORDINATOR_CHECK_NAME", "formal-ui")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}: {} {}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let metadata = evidence(&world);
        assert_eq!(metadata.status, "available", "{metadata:?}");
        assert_eq!(metadata.image_count, 1);
        assert_eq!(
            metadata.bundles[0]
                .coverage
                .required_coverage
                .as_ref()
                .unwrap()
                .entries[0]
                .requirement_id
                .as_deref(),
            Some("required-0001")
        );
        let image = world
            .service
            .image(
                EvidenceImage {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: image_id(&metadata),
                    offset: 0,
                    max_bytes: MAX_IMAGE_CHUNK_BYTES,
                },
                &caller("owner@example.test"),
            )
            .unwrap();
        assert_eq!(
            BASE64.decode(image.base64).unwrap(),
            BASE64.decode(PNG_BASE64).unwrap()
        );
        assert_eq!(image.next_offset, None);
        assert_eq!(
            metadata.bundles[0]
                .coverage
                .required_coverage
                .as_ref()
                .unwrap()
                .satisfied_count,
            1
        );
    }

    #[test]
    fn metadata_is_path_free_and_image_chunks_revalidate_integrity() {
        let world = world();
        let result = evidence(&world);
        assert_eq!(result.status, "available");
        assert_eq!(result.image_count, 1);
        assert!(result.issues.is_empty());
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains(&world.repo.display().to_string())
        );
        let id = image_id(&result);
        let chunk = world
            .service
            .image(
                EvidenceImage {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: id.clone(),
                    offset: 0,
                    max_bytes: MAX_IMAGE_CHUNK_BYTES,
                },
                &caller("owner@example.test"),
            )
            .unwrap();
        assert_eq!(
            BASE64.decode(chunk.base64).unwrap(),
            BASE64.decode(PNG_BASE64).unwrap()
        );
        assert_eq!(chunk.next_offset, None);

        let mut tampered = BASE64.decode(PNG_BASE64).unwrap();
        *tampered.last_mut().unwrap() = b'x';
        fs::write(&world.screenshot, tampered).unwrap();
        let error = world
            .service
            .image(
                EvidenceImage {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: id.clone(),
                    offset: 0,
                    max_bytes: MAX_IMAGE_CHUNK_BYTES,
                },
                &caller("owner@example.test"),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TestEvidenceTampered);
        let error = world
            .service
            .create_feedback(
                CreateFeedback {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: id,
                    body: "This must not attach to changed evidence.".to_owned(),
                    marks: vec![Mark::Pin {
                        id: "mark-1".to_owned(),
                        color: "#ef4444".to_owned(),
                        x: 0.5,
                        y: 0.5,
                    }],
                },
                &caller("owner@example.test"),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TestEvidenceTampered);
        let tasks = world
            .database
            .call(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, u32>(0))
                    .map_err(DatabaseError::from)
            })
            .unwrap();
        assert_eq!(tasks, 0);
    }

    #[test]
    fn required_coverage_preserves_partial_and_failed_proofs_but_rejects_unsafe_fields() {
        let world = world();
        let required = serde_json::json!({"declaredCount":1,"satisfiedCount":1,"failed":false,"entries":[{
            "target":"Sign in","state":"base","viewport":"desktop","width":1280,"status":"satisfied",
            "matchingCellIds":["unselected-plan-cell"],"reason":""
        }]});
        let mut manifest = world.manifest.clone();
        manifest["coverage"]["requiredCoverage"] = serde_json::Value::Null;
        write_manifest(&world.evidence, &manifest);
        assert_eq!(evidence(&world).status, "available");
        manifest["coverage"]["requiredCoverage"] = required;
        manifest["coverage"]["readinessEligible"] = false.into();
        write_manifest(&world.evidence, &manifest);
        assert_eq!(evidence(&world).status, "available");
        manifest["coverage"]["requiredCoverage"]["entries"][0]["requirementId"] =
            "required-0001".into();
        write_manifest(&world.evidence, &manifest);
        let metadata = serde_json::to_value(evidence(&world)).unwrap();
        assert_eq!(
            metadata["bundles"][0]["coverage"]["required_coverage"]["entries"][0]["requirement_id"],
            "required-0001"
        );
        let mut duplicate = manifest.clone();
        let mut second = duplicate["coverage"]["requiredCoverage"]["entries"][0].clone();
        second["target"] = "Different target".into();
        duplicate["coverage"]["requiredCoverage"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(second);
        duplicate["coverage"]["requiredCoverage"]["declaredCount"] = 2.into();
        duplicate["coverage"]["requiredCoverage"]["satisfiedCount"] = 2.into();
        write_manifest(&world.evidence, &duplicate);
        assert_eq!(evidence(&world).status, "unavailable");
        for pointer in [
            "/coverage/requiredCoverage",
            "/coverage/requiredCoverage/entries/0",
        ] {
            let mut unsafe_manifest = manifest.clone();
            unsafe_manifest
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("private_path".into(), "/private/value".into());
            write_manifest(&world.evidence, &unsafe_manifest);
            assert_eq!(evidence(&world).status, "unavailable");
        }
        for (pointer, value) in [
            (
                "/coverage/requiredCoverage/entries/0/requirementId",
                serde_json::json!(""),
            ),
            (
                "/coverage/requiredCoverage/entries/0/requirementId",
                serde_json::json!("x".repeat(129)),
            ),
            (
                "/coverage/requiredCoverage/declaredCount",
                serde_json::json!(2),
            ),
            (
                "/coverage/requiredCoverage/satisfiedCount",
                serde_json::json!(0),
            ),
            ("/coverage/requiredCoverage/failed", serde_json::json!(true)),
            (
                "/coverage/requiredCoverage/entries/0/matchingCellIds",
                serde_json::json!(["duplicate", "duplicate"]),
            ),
            (
                "/coverage/requiredCoverage/entries/0/status",
                serde_json::json!("invented"),
            ),
            (
                "/coverage/requiredCoverage/entries/0/width",
                serde_json::json!(0),
            ),
        ] {
            let mut invalid = manifest.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            write_manifest(&world.evidence, &invalid);
            assert_eq!(evidence(&world).status, "unavailable", "{pointer}");
        }
        manifest["coverage"]["failed"] = true.into();
        manifest["coverage"]["requiredCoverage"]["failed"] = true.into();
        manifest["coverage"]["requiredCoverage"]["satisfiedCount"] = 0.into();
        manifest["coverage"]["requiredCoverage"]["entries"][0]["status"] = "missing".into();
        manifest["coverage"]["requiredCoverage"]["entries"][0]["matchingCellIds"] =
            serde_json::json!([]);
        manifest["coverage"]["requiredCoverage"]["entries"][0]["reason"] =
            "required cell missing".into();
        write_manifest(&world.evidence, &manifest);
        let result = evidence(&world);
        assert_eq!(result.status, "available");
        assert!(result.bundles[0].coverage.failed);
        assert!(
            result.bundles[0]
                .coverage
                .required_coverage
                .as_ref()
                .unwrap()
                .failed
        );
        assert!(!result.bundles[0].coverage.readiness_eligible);
    }

    #[test]
    fn multiple_bundles_are_sorted_and_unsafe_directories_are_ignored() {
        let world = world();
        let bundle = world.evidence.join("formal-runs").join("b".repeat(64));
        fs::create_dir_all(bundle.join("screenshots")).unwrap();
        fs::write(
            bundle.join("screenshots/second-viewport.png"),
            BASE64.decode(PNG_BASE64).unwrap(),
        )
        .unwrap();
        let mut manifest = world.manifest.clone();
        manifest["runId"] = json!("formal-web-ui-second");
        manifest["cells"][0]["cellId"] = json!("cell-2");
        manifest["cells"][0]["targetName"] = json!("Account [base]");
        manifest["cells"][0]["screenshots"]["viewport"]["path"] =
            json!("screenshots/second-viewport.png");
        write_manifest(&bundle, &manifest);

        let outside = world.repo.join("outside");
        write_manifest(&outside, &world.manifest);
        let linked = world.evidence.join("formal-runs").join("c".repeat(64));
        symlink(&outside, &linked).unwrap();
        let unrecognized = world.evidence.join("formal-runs/not-a-bundle");
        write_manifest(&unrecognized, &world.manifest);

        let result = evidence(&world);
        assert_eq!(
            result
                .bundles
                .iter()
                .map(|bundle| bundle.formal_run_id.as_str())
                .collect::<Vec<_>>(),
            ["formal-web-ui-example", "formal-web-ui-second"]
        );
        assert_eq!(result.image_count, 2);
        assert!(result.issues.is_empty());
        let summary = world
            .service
            .summary_registered(&world.repo, RUN_ID)
            .unwrap();
        assert_eq!(summary.bundle_count, 2);
        assert_eq!(summary.image_count, 2);
    }

    #[test]
    fn invalid_and_expired_evidence_report_truthfully() {
        let world = world();
        let mut invalid = world.manifest.clone();
        invalid["governedRunId"] = json!("another-run");
        write_manifest(&world.evidence, &invalid);
        let result = evidence(&world);
        assert_eq!(result.status, "unavailable");
        assert_eq!(result.issues[0].code, "invalid_evidence");

        let error = world
            .service
            .get(
                EvidenceReference {
                    path: world.repo.display().to_string(),
                    run_id: "t20260902T020304Z-fedcba".to_owned(),
                },
                &caller("owner@example.test"),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TestEvidenceExpired);
    }

    #[test]
    fn annotation_and_linked_plan_task_share_one_complete_lifecycle() {
        let world = world();
        let id = image_id(&evidence(&world));
        let owner = caller("owner@example.test");
        let created = world
            .service
            .create_feedback(
                CreateFeedback {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: id,
                    body: "The sign-in button needs stronger contrast.".to_owned(),
                    marks: vec![
                        Mark::Rectangle {
                            id: "mark-1".to_owned(),
                            color: "#F59E0B".to_owned(),
                            x: 0.2,
                            y: 0.3,
                            width: 0.4,
                            height: 0.1,
                        },
                        Mark::Pin {
                            id: "mark-2".to_owned(),
                            color: "#4c8dff".to_owned(),
                            x: 0.5,
                            y: 0.5,
                        },
                        Mark::Arrow {
                            id: "mark-3".to_owned(),
                            color: "#ef4444".to_owned(),
                            x1: 0.1,
                            y1: 0.1,
                            x2: 0.4,
                            y2: 0.4,
                        },
                        Mark::Freehand {
                            id: "mark-4".to_owned(),
                            color: "#22c55e".to_owned(),
                            points: vec![Point { x: 0.1, y: 0.7 }, Point { x: 0.4, y: 0.8 }],
                        },
                        Mark::Highlight {
                            id: "mark-5".to_owned(),
                            color: "#a855f7".to_owned(),
                            points: vec![Point { x: 0.2, y: 0.6 }, Point { x: 0.7, y: 0.6 }],
                        },
                        Mark::Text {
                            id: "mark-6".to_owned(),
                            color: "#f8fafc".to_owned(),
                            x: 0.3,
                            y: 0.3,
                            text: "X".to_owned(),
                        },
                    ],
                },
                &owner,
            )
            .unwrap();
        assert_eq!(created.feedback.task_status, TaskStatus::Planned);
        assert_eq!(created.feedback.marks.len(), 6);
        let root_comment = created.feedback.comments[0].comment_id.clone();
        let task = world
            .database
            .call({
                let task_id = created.task_id.clone();
                move |connection| {
                    connection
                        .query_row(
                            "SELECT kind,status,title FROM tasks WHERE task_id=?1",
                            [task_id],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .map_err(DatabaseError::from)
                }
            })
            .unwrap();
        assert_eq!(task.0, "user_feedback");
        assert_eq!(task.1, "planned");
        assert_eq!(
            task.2,
            "Review: The sign-in button needs stronger contrast."
        );

        let replied = world
            .service
            .reply(
                FeedbackReply {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id.clone(),
                    body: "Please use the normal primary action treatment.".to_owned(),
                },
                &owner,
            )
            .unwrap();
        assert_eq!(replied.feedback.comments.len(), 2);
        let edited = world
            .service
            .edit(
                FeedbackEdit {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id.clone(),
                    comment_id: root_comment,
                    body: "The sign-in button needs the standard primary contrast.".to_owned(),
                },
                &owner,
            )
            .unwrap();
        assert!(edited.feedback.comments[0].body.ends_with("contrast."));
        let resolved = world
            .service
            .set_state(
                FeedbackStateChange {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id.clone(),
                    state: FeedbackState::Resolved,
                },
                &owner,
            )
            .unwrap();
        assert_eq!(resolved.feedback.state, "resolved");
        assert_eq!(resolved.feedback.task_status, TaskStatus::Done);
        let reopened = world
            .service
            .set_state(
                FeedbackStateChange {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id.clone(),
                    state: FeedbackState::Open,
                },
                &owner,
            )
            .unwrap();
        assert_eq!(reopened.feedback.state, "open");

        let error = world
            .service
            .delete(
                FeedbackDelete {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id.clone(),
                },
                &caller("other@example.test"),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);
        let deleted = world
            .service
            .delete(
                FeedbackDelete {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    feedback_id: created.feedback_id,
                },
                &owner,
            )
            .unwrap();
        assert_eq!(deleted.feedback.state, "deleted");
        assert_eq!(deleted.feedback.task_status, TaskStatus::Dropped);
        let events = world
            .database
            .call(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM visual_feedback_events", [], |row| {
                        row.get::<_, u32>(0)
                    })
                    .map_err(DatabaseError::from)
            })
            .unwrap();
        assert!(events >= 6);
    }

    #[test]
    fn strict_manifest_and_mark_validation_rejects_ambiguous_input() {
        let world = world();
        let mut missing = world.manifest.clone();
        missing["cells"][0]
            .as_object_mut()
            .unwrap()
            .remove("primaryJourney");
        write_manifest(&world.evidence, &missing);
        let result = evidence(&world);
        assert_eq!(result.status, "unavailable");
        assert_eq!(result.issues[0].code, "invalid_evidence");

        assert!(
            validate_marks(vec![Mark::Rectangle {
                id: "mark-1".to_owned(),
                color: "#ef4444".to_owned(),
                x: 0.9,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            }])
            .is_err()
        );
        assert!(
            validate_marks(vec![
                Mark::Pin {
                    id: "same".to_owned(),
                    color: "#ef4444".to_owned(),
                    x: 0.1,
                    y: 0.1,
                },
                Mark::Pin {
                    id: "same".to_owned(),
                    color: "#ef4444".to_owned(),
                    x: 0.2,
                    y: 0.2,
                },
            ])
            .is_err()
        );
    }

    #[test]
    fn current_run_list_is_enriched_without_repeating_repository_discovery() {
        let world = world();
        let summary = crate::test_state::initial_summary(
            RUN_ID,
            "all",
            "2026-09-02T01:02:58Z",
            1000,
            "codex",
            ProofKind::Complete,
            Vec::new(),
            None,
            ValidationTier::Release,
        );
        assert_eq!(summary.status, TestStatus::Running);
        let mut list = TestList {
            next_worktree_id: None,
            runs: vec![TestListRow {
                worktree_id: world.worktree_id,
                worktree_path: world.repo.display().to_string(),
                repository_id: world.repository_id,
                display_name: "repo".to_owned(),
                repository_source: None,
                earlier_visual_evidence: None,
                visual_evidence: VisualEvidenceSummary {
                    status: "unavailable".to_owned(),
                    bundle_count: 0,
                    image_count: 0,
                    issue_count: 0,
                    issues_truncated: false,
                    error_code: None,
                },
                summary,
            }],
        };
        world.service.enrich_list(&mut list);
        assert_eq!(list.runs[0].visual_evidence.status, "available");
        assert_eq!(list.runs[0].visual_evidence.bundle_count, 1);
        assert_eq!(list.runs[0].visual_evidence.image_count, 1);
        assert_eq!(list.runs[0].visual_evidence.error_code, None);
        assert!(list.runs[0].earlier_visual_evidence.is_none());
        let mut earlier_summary = list.runs[0].summary.clone();
        earlier_summary.status = TestStatus::Passed;
        earlier_summary.finished_at = Some("2026-09-02T01:03:00Z".to_owned());
        crate::test_state::TestRunStore
            .record_history(
                &world.repo,
                &earlier_summary,
                rustix::process::getuid().as_raw(),
                rustix::process::getgid().as_raw(),
            )
            .unwrap();
        list.runs[0].summary.run_id = "t20260902T020000Z-abcdef".to_owned();
        world.service.enrich_list(&mut list);
        assert_eq!(list.runs[0].visual_evidence.image_count, 0);
        let earlier = list.runs[0].earlier_visual_evidence.as_ref().unwrap();
        assert_eq!(earlier.run_id, RUN_ID);
        assert_eq!(earlier.started_at, earlier_summary.started_at);
        assert_eq!(earlier.visual_evidence.image_count, 1);
    }

    #[test]
    fn concurrent_replies_receive_one_lossless_sequence() {
        let world = world();
        let created = world
            .service
            .create_feedback(
                CreateFeedback {
                    path: world.repo.display().to_string(),
                    run_id: RUN_ID.to_owned(),
                    image_id: image_id(&evidence(&world)),
                    body: "Keep every concurrent reply in this discussion.".to_owned(),
                    marks: vec![Mark::Pin {
                        id: "mark-1".to_owned(),
                        color: "#ef4444".to_owned(),
                        x: 0.5,
                        y: 0.5,
                    }],
                },
                &caller("owner@example.test"),
            )
            .unwrap();
        let mut replies = Vec::new();
        for index in 0..8 {
            let service = world.service.clone();
            let path = world.repo.display().to_string();
            let feedback_id = created.feedback_id.clone();
            replies.push(std::thread::spawn(move || {
                service.reply(
                    FeedbackReply {
                        path,
                        run_id: RUN_ID.to_owned(),
                        feedback_id,
                        body: format!("Concurrent visual feedback reply {index}."),
                    },
                    &caller("owner@example.test"),
                )
            }));
        }
        for reply in replies {
            reply.join().unwrap().unwrap();
        }
        let result = evidence(&world);
        assert_eq!(result.feedback[0].comments.len(), 9);
        let sequences = world
            .database
            .call({
                let feedback_id = created.feedback_id;
                move |connection| {
                    let mut statement = connection.prepare(
                        "SELECT seq FROM visual_feedback_comments WHERE feedback_id=?1 ORDER BY seq",
                    )?;
                    Ok(statement
                        .query_map([feedback_id], |row| row.get::<_, u32>(0))?
                        .collect::<Result<Vec<_>, _>>()?)
                }
            })
            .unwrap();
        assert_eq!(sequences, (1..=9).collect::<Vec<_>>());
    }
}
