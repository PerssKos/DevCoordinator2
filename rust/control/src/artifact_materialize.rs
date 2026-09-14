//! Verified, descriptor-relative materialization of retained artifact trees.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use devcoordinator2_api::params::ValidationTier;
use devcoordinator2_api::results::{
    ArtifactCatalog, ArtifactChunk, ArtifactEntry, ArtifactSummary, ProofKind,
};
use devcoordinator2_api::{ClientContext, ErrorCode, ProtocolError, ResponseEnvelope};
use rustix::fs::{self as unix_fs, AtFlags, Dir, Mode, OFlags};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client;

const MAX_ARTIFACTS: usize = 8;
const MAX_FILES: usize = 4_096;
const MAX_ARTIFACT_BYTES: u64 = 1_024 * 1_024 * 1_024;
const MAX_TOTAL_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;
const MAX_PAGE: usize = 100;
const MAX_CHUNK_BYTES: u32 = 180 * 1_024;
const TREE_DOMAIN: &[u8] = b"devcoordinator2-retained-artifact-tree-v1\0";
const MAX_ERROR_MESSAGE_BYTES: usize = 1_024;
const MAX_ERROR_DETAIL_BYTES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializeRequest {
    pub path: PathBuf,
    pub run_id: String,
    pub check: String,
    /// Empty means every artifact in the manifest, preserving manifest order.
    pub artifacts: Vec<String>,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializeReceipt {
    pub run_id: String,
    pub check: String,
    pub test: String,
    pub requested_tier: ValidationTier,
    pub readiness_eligible: bool,
    pub proof: ProofKind,
    pub source_sha256: String,
    pub config_sha256: String,
    pub run_status: String,
    pub run_complete: bool,
    pub run_finished_at_epoch_ms: Option<u64>,
    pub run_metadata_sha256: String,
    pub manifest_sha256: String,
    pub destination: String,
    pub artifacts: Vec<MaterializedArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedArtifact {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub files: u32,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitClassification {
    /// Local input/transport/protocol failure: CLI exit 2.
    Transport,
    /// Daemon operation failure or local evidence-verification failure: exit 1.
    Operation,
}

impl ExitClassification {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Transport => 2,
            Self::Operation => 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializeError {
    pub classification: ExitClassification,
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl MaterializeError {
    pub const fn exit_code(&self) -> u8 {
        self.classification.exit_code()
    }

    fn new(
        classification: ExitClassification,
        code: ErrorCode,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            classification,
            code,
            message: truncate_utf8(&message.into(), MAX_ERROR_MESSAGE_BYTES),
            detail: truncate_utf8(&detail.into(), MAX_ERROR_DETAIL_BYTES),
        }
    }

    fn input(message: impl Into<String>) -> Self {
        Self::new(
            ExitClassification::Transport,
            ErrorCode::ParamsInvalid,
            message,
            "",
        )
    }

    fn transport(error: ProtocolError) -> Self {
        Self::new(
            ExitClassification::Transport,
            error.code,
            error.message,
            error.detail,
        )
    }

    fn operation(code: ErrorCode, message: impl Into<String>, detail: impl Into<String>) -> Self {
        let classification = if code == ErrorCode::DaemonUnavailable {
            ExitClassification::Transport
        } else {
            ExitClassification::Operation
        };
        Self::new(classification, code, message, detail)
    }

    fn tampered(message: impl Into<String>) -> Self {
        Self::operation(ErrorCode::TestArtifactTampered, message, "")
    }

    fn tampered_detail(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::operation(ErrorCode::TestArtifactTampered, message, detail)
    }

    fn with_cleanup(mut self, cleanup: impl Into<String>) -> Self {
        let cleanup = cleanup.into();
        if !cleanup.is_empty() {
            let detail = if self.detail.is_empty() {
                format!("cleanup: {cleanup}")
            } else {
                format!("{}; cleanup: {cleanup}", self.detail)
            };
            self.detail = truncate_utf8(&detail, MAX_ERROR_DETAIL_BYTES);
        }
        self
    }
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MaterializeError {}

pub type V2CallFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResponseEnvelope, ProtocolError>> + Send + 'a>>;

/// Injectable protocol-v2 call boundary used by materialization and tests.
pub trait V2Call: Send + Sync {
    fn call<'a>(&'a self, operation: &'static str, params: Value) -> V2CallFuture<'a>;
}

#[derive(Clone, Debug)]
pub struct ClientV2Call {
    socket_path: PathBuf,
    client: ClientContext,
}

impl ClientV2Call {
    pub fn new(socket_path: PathBuf, client: ClientContext) -> Self {
        Self {
            socket_path,
            client,
        }
    }
}

impl V2Call for ClientV2Call {
    fn call<'a>(&'a self, operation: &'static str, params: Value) -> V2CallFuture<'a> {
        Box::pin(async move {
            client::call(&self.socket_path, operation, params, self.client.clone()).await
        })
    }
}

pub async fn materialize_with_client(
    request: MaterializeRequest,
    socket_path: PathBuf,
    context: ClientContext,
) -> Result<MaterializeReceipt, MaterializeError> {
    materialize(request, &ClientV2Call::new(socket_path, context)).await
}

pub async fn materialize<C: V2Call + ?Sized>(
    request: MaterializeRequest,
    caller: &C,
) -> Result<MaterializeReceipt, MaterializeError> {
    let source_path = validate_request(&request)?;
    let destination = DestinationPlan::prepare(&request.destination)?;
    let root: ArtifactCatalog = call_typed(
        caller,
        "test.artifact.catalog",
        json!({
            "path":source_path,
            "run_id":request.run_id,
            "check":request.check,
            "offset":0,
            "limit":MAX_PAGE
        }),
    )
    .await?;
    validate_root_catalog(&root, &request)?;
    let selected = select_artifacts(&root, &request.artifacts)?;
    let mut destination = destination.create()?;
    let result = materialize_selected(
        caller,
        &request,
        &source_path,
        &root,
        &selected,
        &mut destination,
    )
    .await;
    match result {
        Ok(artifacts) => {
            if let Err(error) = destination.commit() {
                let cleanup = destination.cleanup().err().map(|error| error.to_string());
                return Err(match cleanup {
                    Some(cleanup) => error.with_cleanup(cleanup),
                    None => error,
                });
            }
            Ok(MaterializeReceipt {
                run_id: root.run_id,
                check: root.check,
                test: root.test,
                requested_tier: root.requested_tier,
                readiness_eligible: root.readiness_eligible,
                proof: root.proof,
                source_sha256: root.source_sha256,
                config_sha256: root.config_sha256,
                run_status: root.run_status,
                run_complete: root.run_complete,
                run_finished_at_epoch_ms: root.run_finished_at_epoch_ms,
                run_metadata_sha256: root.run_metadata_sha256,
                manifest_sha256: root.manifest_sha256,
                destination: request
                    .destination
                    .to_str()
                    .expect("request validation required UTF-8")
                    .to_owned(),
                artifacts,
            })
        }
        Err(error) => {
            let cleanup = destination.cleanup().err().map(|error| error.to_string());
            Err(match cleanup {
                Some(cleanup) => error.with_cleanup(cleanup),
                None => error,
            })
        }
    }
}

async fn materialize_selected<C: V2Call + ?Sized>(
    caller: &C,
    request: &MaterializeRequest,
    source_path: &str,
    root: &ArtifactCatalog,
    selected: &[ArtifactSummary],
    destination: &mut DestinationGuard,
) -> Result<Vec<MaterializedArtifact>, MaterializeError> {
    let mut materialized = Vec::with_capacity(selected.len());
    for summary in selected {
        let entries = page_entries(caller, request, source_path, root, summary).await?;
        verify_tree_receipt(summary, &entries)?;
        let artifact_directory = destination.create_artifact_directory(&summary.name)?;
        for entry in &entries {
            materialize_file(
                caller,
                request,
                source_path,
                root,
                summary,
                entry,
                &artifact_directory,
            )
            .await?;
        }
        artifact_directory.sync_all().map_err(|error| {
            MaterializeError::tampered_detail(
                "could not sync the materialized artifact directory",
                error.to_string(),
            )
        })?;
        materialized.push(MaterializedArtifact {
            name: summary.name.clone(),
            path: summary.name.clone(),
            size: summary.size,
            files: summary.files,
            sha256: summary.sha256.clone(),
        });
    }
    Ok(materialized)
}

async fn page_entries<C: V2Call + ?Sized>(
    caller: &C,
    request: &MaterializeRequest,
    source_path: &str,
    root: &ArtifactCatalog,
    summary: &ArtifactSummary,
) -> Result<Vec<ArtifactEntry>, MaterializeError> {
    let mut entries = Vec::with_capacity(summary.files as usize);
    let mut offset = 0_u32;
    loop {
        let page: ArtifactCatalog = call_typed(
            caller,
            "test.artifact.catalog",
            json!({
                "path":source_path,
                "run_id":request.run_id,
                "check":request.check,
                "artifact":summary.name,
                "manifest_sha256":root.manifest_sha256,
                "offset":offset,
                "limit":MAX_PAGE
            }),
        )
        .await?;
        validate_page_identity(root, summary, &page)?;
        if page.entries.len() > MAX_PAGE {
            return Err(MaterializeError::tampered(
                "retained artifact page exceeds its public bound",
            ));
        }
        if entries.len() + page.entries.len() > MAX_FILES {
            return Err(MaterializeError::tampered(
                "retained artifact file count exceeds its public bound",
            ));
        }
        for entry in &page.entries {
            validate_entry(entry)?;
            if entries
                .last()
                .is_some_and(|previous: &ArtifactEntry| previous.path >= entry.path)
            {
                return Err(MaterializeError::tampered(
                    "retained artifact entries are not uniquely sorted",
                ));
            }
            entries.push(entry.clone());
        }
        let consumed = u32::try_from(page.entries.len())
            .map_err(|_| MaterializeError::tampered("retained artifact page count is invalid"))?;
        let expected_next = offset.checked_add(consumed).ok_or_else(|| {
            MaterializeError::tampered("retained artifact page cursor overflowed")
        })?;
        match page.next_offset {
            Some(next) => {
                if consumed == 0 || next != expected_next || next > summary.files {
                    return Err(MaterializeError::tampered(
                        "retained artifact page cursor is invalid",
                    ));
                }
                offset = next;
            }
            None => break,
        }
    }
    if entries.len() != summary.files as usize {
        return Err(MaterializeError::tampered(
            "retained artifact catalogue omitted files",
        ));
    }
    Ok(entries)
}

async fn materialize_file<C: V2Call + ?Sized>(
    caller: &C,
    request: &MaterializeRequest,
    source_path: &str,
    root: &ArtifactCatalog,
    summary: &ArtifactSummary,
    entry: &ArtifactEntry,
    artifact_directory: &std::fs::File,
) -> Result<(), MaterializeError> {
    let (parent, name) = create_entry_parent(artifact_directory, &entry.path)?;
    let descriptor = unix_fs::openat(
        &parent,
        name.as_os_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| {
        MaterializeError::tampered_detail(
            "could not create a materialized artifact file",
            error.to_string(),
        )
    })?;
    let mut file = std::fs::File::from(descriptor);
    unix_fs::fchmod(&file, Mode::from_raw_mode(0o600)).map_err(|error| {
        MaterializeError::tampered_detail(
            "could not set materialized artifact file permissions",
            error.to_string(),
        )
    })?;
    let mut digest = Sha256::new();
    let mut written = 0_u64;
    loop {
        let chunk: ArtifactChunk = call_typed(
            caller,
            "test.artifact.file",
            json!({
                "path":source_path,
                "run_id":request.run_id,
                "check":request.check,
                "artifact":summary.name,
                "file":entry.path,
                "manifest_sha256":root.manifest_sha256,
                "offset":written,
                "max_bytes":MAX_CHUNK_BYTES
            }),
        )
        .await?;
        validate_chunk_identity(&chunk, request, summary, entry, written)?;
        let payload = decode_base64(&chunk.base64).map_err(|detail| {
            MaterializeError::tampered_detail("retained artifact chunk is not valid base64", detail)
        })?;
        if payload.len() != chunk.bytes as usize || payload.len() > MAX_CHUNK_BYTES as usize {
            return Err(MaterializeError::tampered(
                "retained artifact chunk length differs",
            ));
        }
        file.write_all(&payload).map_err(|error| {
            MaterializeError::tampered_detail(
                "could not write a materialized artifact file",
                error.to_string(),
            )
        })?;
        digest.update(&payload);
        written = written.checked_add(payload.len() as u64).ok_or_else(|| {
            MaterializeError::tampered("retained artifact chunk length overflowed")
        })?;
        match chunk.next_offset {
            Some(next) => {
                if payload.is_empty() || next != written || next >= entry.size {
                    return Err(MaterializeError::tampered(
                        "retained artifact chunk cursor differs",
                    ));
                }
            }
            None => {
                if written != entry.size {
                    return Err(MaterializeError::tampered(
                        "retained artifact file ended before its receipt",
                    ));
                }
                break;
            }
        }
    }
    file.sync_all().map_err(|error| {
        MaterializeError::tampered_detail(
            "could not sync a materialized artifact file",
            error.to_string(),
        )
    })?;
    if written != entry.size || lower_hex(digest.finalize().as_slice()) != entry.sha256 {
        return Err(MaterializeError::tampered(
            "materialized artifact file hash differs",
        ));
    }
    parent.sync_all().map_err(|error| {
        MaterializeError::tampered_detail(
            "could not sync a materialized artifact directory",
            error.to_string(),
        )
    })?;
    Ok(())
}

async fn call_typed<C, T>(
    caller: &C,
    operation: &'static str,
    params: Value,
) -> Result<T, MaterializeError>
where
    C: V2Call + ?Sized,
    T: DeserializeOwned,
{
    let response = caller
        .call(operation, params)
        .await
        .map_err(MaterializeError::transport)?;
    match response {
        ResponseEnvelope::Success { data, .. } => serde_json::from_value(data).map_err(|error| {
            MaterializeError::tampered_detail(
                "Coordinator returned an invalid artifact response",
                error.to_string(),
            )
        }),
        ResponseEnvelope::Failure { error, .. } => Err(MaterializeError::operation(
            error.code,
            error.message,
            error.detail,
        )),
    }
}

fn validate_request(request: &MaterializeRequest) -> Result<String, MaterializeError> {
    if !request.path.is_absolute() {
        return Err(MaterializeError::input(
            "artifact source path must be absolute",
        ));
    }
    let source_path = request
        .path
        .to_str()
        .ok_or_else(|| MaterializeError::input("artifact source path must be valid UTF-8"))?
        .to_owned();
    if !valid_run_id(&request.run_id) {
        return Err(MaterializeError::input("run_id is invalid"));
    }
    if !valid_simple_name(&request.check, 64) {
        return Err(MaterializeError::input("check is invalid"));
    }
    Ok(source_path)
}

fn validate_root_catalog(
    root: &ArtifactCatalog,
    request: &MaterializeRequest,
) -> Result<(), MaterializeError> {
    if !valid_prefixed_hex(&root.repository_id, 'r')
        || !valid_prefixed_hex(&root.worktree_id, 'w')
        || root.run_id != request.run_id
        || root.check != request.check
    {
        return Err(MaterializeError::tampered(
            "retained artifact catalogue identity differs",
        ));
    }
    if !valid_sha256(&root.manifest_sha256)
        || !valid_sha256(&root.source_sha256)
        || !valid_sha256(&root.config_sha256)
        || !valid_sha256(&root.run_metadata_sha256)
        || !valid_simple_name(&root.test, 32)
        || !matches!(root.run_status.as_str(), "running" | "passed" | "failed")
        || root.artifacts.is_empty()
        || root.artifacts.len() > MAX_ARTIFACTS
        || root.artifact.is_some()
        || !root.entries.is_empty()
        || root.next_offset.is_some()
    {
        return Err(MaterializeError::tampered(
            "retained artifact catalogue metadata is invalid",
        ));
    }
    let mut names = HashSet::new();
    let mut total = 0_u64;
    for artifact in &root.artifacts {
        validate_summary(artifact)?;
        if !names.insert(&artifact.name) {
            return Err(MaterializeError::tampered(
                "retained artifact names are repeated",
            ));
        }
        total = total
            .checked_add(artifact.size)
            .ok_or_else(|| MaterializeError::tampered("retained artifact size overflowed"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(MaterializeError::tampered(
                "retained artifacts exceed their combined bound",
            ));
        }
    }
    Ok(())
}

fn validate_summary(summary: &ArtifactSummary) -> Result<(), MaterializeError> {
    if !valid_simple_name(&summary.name, 64)
        || summary.size > MAX_ARTIFACT_BYTES
        || summary.files == 0
        || summary.files as usize > MAX_FILES
        || !valid_sha256(&summary.sha256)
    {
        return Err(MaterializeError::tampered(
            "retained artifact summary is invalid",
        ));
    }
    Ok(())
}

fn select_artifacts(
    root: &ArtifactCatalog,
    requested: &[String],
) -> Result<Vec<ArtifactSummary>, MaterializeError> {
    if requested.is_empty() {
        return Ok(root.artifacts.clone());
    }
    let available = root
        .artifacts
        .iter()
        .map(|artifact| (artifact.name.as_str(), artifact))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::with_capacity(requested.len());
    let mut names = HashSet::new();
    for name in requested {
        if !valid_simple_name(name, 64) || !names.insert(name.as_str()) {
            return Err(MaterializeError::tampered(
                "requested retained artifact name is missing or repeated",
            ));
        }
        let summary = available.get(name.as_str()).ok_or_else(|| {
            MaterializeError::tampered("requested retained artifact name is missing or repeated")
        })?;
        selected.push((*summary).clone());
    }
    Ok(selected)
}

fn validate_page_identity(
    root: &ArtifactCatalog,
    summary: &ArtifactSummary,
    page: &ArtifactCatalog,
) -> Result<(), MaterializeError> {
    if page.repository_id != root.repository_id
        || page.worktree_id != root.worktree_id
        || page.run_id != root.run_id
        || page.check != root.check
        || page.manifest_sha256 != root.manifest_sha256
        || page.test != root.test
        || page.requested_tier != root.requested_tier
        || page.readiness_eligible != root.readiness_eligible
        || page.proof != root.proof
        || page.source_sha256 != root.source_sha256
        || page.config_sha256 != root.config_sha256
        || page.run_status != root.run_status
        || page.run_complete != root.run_complete
        || page.run_finished_at_epoch_ms != root.run_finished_at_epoch_ms
        || page.run_metadata_sha256 != root.run_metadata_sha256
        || page.artifacts != root.artifacts
        || page.artifact.as_ref() != Some(summary)
    {
        return Err(MaterializeError::tampered(
            "retained artifact page identity differs",
        ));
    }
    Ok(())
}

fn validate_entry(entry: &ArtifactEntry) -> Result<(), MaterializeError> {
    if !valid_relative_file(&entry.path)
        || entry.size > MAX_ARTIFACT_BYTES
        || !valid_sha256(&entry.sha256)
    {
        return Err(MaterializeError::tampered(
            "retained artifact file descriptor is invalid",
        ));
    }
    Ok(())
}

fn verify_tree_receipt(
    summary: &ArtifactSummary,
    entries: &[ArtifactEntry],
) -> Result<(), MaterializeError> {
    if entries.len() != summary.files as usize {
        return Err(MaterializeError::tampered(
            "retained artifact catalogue does not match its tree receipt",
        ));
    }
    let mut total = 0_u64;
    for entry in entries {
        total = total
            .checked_add(entry.size)
            .ok_or_else(|| MaterializeError::tampered("retained artifact tree size overflowed"))?;
        if total > MAX_ARTIFACT_BYTES {
            return Err(MaterializeError::tampered(
                "retained artifact tree exceeds its size bound",
            ));
        }
    }
    if total != summary.size || tree_digest(entries) != summary.sha256 {
        return Err(MaterializeError::tampered(
            "retained artifact catalogue does not match its tree receipt",
        ));
    }
    Ok(())
}

fn validate_chunk_identity(
    chunk: &ArtifactChunk,
    request: &MaterializeRequest,
    summary: &ArtifactSummary,
    entry: &ArtifactEntry,
    offset: u64,
) -> Result<(), MaterializeError> {
    let maximum_encoded = (MAX_CHUNK_BYTES as usize).div_ceil(3) * 4;
    if chunk.run_id != request.run_id
        || chunk.check != request.check
        || chunk.artifact != summary.name
        || chunk.file != entry.path
        || chunk.sha256 != entry.sha256
        || chunk.total_bytes != entry.size
        || chunk.offset != offset
        || chunk.bytes > MAX_CHUNK_BYTES
        || chunk.base64.len() > maximum_encoded
    {
        return Err(MaterializeError::tampered(
            "retained artifact chunk identity differs",
        ));
    }
    Ok(())
}

fn tree_digest(entries: &[ArtifactEntry]) -> String {
    let mut digest = Sha256::new();
    digest.update(TREE_DOMAIN);
    for entry in entries {
        digest.update(entry.path.as_bytes());
        digest.update([0]);
        digest.update(entry.size.to_string().as_bytes());
        digest.update([0]);
        digest.update(entry.sha256.as_bytes());
        digest.update([0]);
    }
    lower_hex(digest.finalize().as_slice())
}

fn valid_prefixed_hex(value: &str, prefix: char) -> bool {
    value.len() == 17
        && value.starts_with(prefix)
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_run_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_simple_name(value: &str, maximum: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= maximum
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_relative_file(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.bytes().any(|byte| byte < 32 || byte == 127)
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn lower_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

struct DestinationPlan {
    parent: std::fs::File,
    name: OsString,
}

impl DestinationPlan {
    fn prepare(path: &Path) -> Result<Self, MaterializeError> {
        if !path.is_absolute() {
            return Err(MaterializeError::input("--destination must be absolute"));
        }
        if path.to_str().is_none() {
            return Err(MaterializeError::input("--destination must be valid UTF-8"));
        }
        let parent_path = path
            .parent()
            .ok_or_else(|| MaterializeError::input("destination must have an existing parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| MaterializeError::input("destination must name a new directory"))?;
        if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
            return Err(MaterializeError::input(
                "destination must name a new directory",
            ));
        }
        let parent = open_absolute_directory(parent_path).map_err(|error| {
            MaterializeError::new(
                ExitClassification::Transport,
                ErrorCode::ParamsInvalid,
                "destination parent is unavailable without following links",
                error.to_string(),
            )
        })?;
        match unix_fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => {
                return Err(MaterializeError::input(
                    "destination must not already exist",
                ));
            }
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Err(error) => {
                return Err(MaterializeError::new(
                    ExitClassification::Transport,
                    ErrorCode::ParamsInvalid,
                    "destination cannot be inspected safely",
                    error.to_string(),
                ));
            }
        }
        Ok(Self {
            parent,
            name: name.to_owned(),
        })
    }

    fn create(self) -> Result<DestinationGuard, MaterializeError> {
        unix_fs::mkdirat(&self.parent, &self.name, Mode::from_raw_mode(0o700)).map_err(
            |error| {
                MaterializeError::tampered_detail(
                    "could not create the materialization destination",
                    error.to_string(),
                )
            },
        )?;
        let descriptor = match unix_fs::openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                // The directory was just created. Remove it only through the
                // pinned parent and only while it is still an empty directory.
                let _ = unix_fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
                return Err(MaterializeError::tampered_detail(
                    "could not open the new materialization destination safely",
                    error.to_string(),
                ));
            }
        };
        let root = std::fs::File::from(descriptor);
        let identity = match file_identity(&root) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = unix_fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR);
                return Err(MaterializeError::tampered_detail(
                    "could not bind the materialization destination identity",
                    error.to_string(),
                ));
            }
        };
        let mut guard = DestinationGuard {
            parent: self.parent,
            root,
            name: self.name,
            identity,
            active: true,
        };
        if let Err(error) = unix_fs::fchmod(&guard.root, Mode::from_raw_mode(0o700)) {
            let result = MaterializeError::tampered_detail(
                "could not set materialization destination permissions",
                error.to_string(),
            );
            return Err(match guard.cleanup() {
                Ok(()) => result,
                Err(cleanup) => result.with_cleanup(cleanup.to_string()),
            });
        }
        Ok(guard)
    }
}

struct DestinationGuard {
    parent: std::fs::File,
    root: std::fs::File,
    name: OsString,
    identity: (u64, u64),
    active: bool,
}

impl DestinationGuard {
    fn create_artifact_directory(&self, name: &str) -> Result<std::fs::File, MaterializeError> {
        unix_fs::mkdirat(&self.root, name, Mode::from_raw_mode(0o700)).map_err(|error| {
            MaterializeError::tampered_detail(
                "could not create a materialized artifact directory",
                error.to_string(),
            )
        })?;
        let descriptor = unix_fs::openat(
            &self.root,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| {
            MaterializeError::tampered_detail(
                "could not open a materialized artifact directory safely",
                error.to_string(),
            )
        })?;
        let directory = std::fs::File::from(descriptor);
        unix_fs::fchmod(&directory, Mode::from_raw_mode(0o700)).map_err(|error| {
            MaterializeError::tampered_detail(
                "could not set materialized artifact directory permissions",
                error.to_string(),
            )
        })?;
        Ok(directory)
    }

    fn verify_link(&self) -> Result<(), MaterializeError> {
        let descriptor = unix_fs::openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| {
            MaterializeError::tampered_detail(
                "materialization destination link changed",
                error.to_string(),
            )
        })?;
        let linked = std::fs::File::from(descriptor);
        if file_identity(&linked).map_err(|error| {
            MaterializeError::tampered_detail(
                "could not revalidate materialization destination",
                error.to_string(),
            )
        })? != self.identity
        {
            return Err(MaterializeError::tampered(
                "materialization destination identity changed",
            ));
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), MaterializeError> {
        self.verify_link()?;
        self.root.sync_all().map_err(|error| {
            MaterializeError::tampered_detail(
                "could not sync the materialization destination",
                error.to_string(),
            )
        })?;
        self.parent.sync_all().map_err(|error| {
            MaterializeError::tampered_detail(
                "could not sync the materialization parent",
                error.to_string(),
            )
        })?;
        self.active = false;
        Ok(())
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        remove_contents(&self.root)?;
        let descriptor = unix_fs::openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let linked = std::fs::File::from(descriptor);
        if file_identity(&linked)? != self.identity {
            return Err(io::Error::other(
                "destination link no longer names the created directory",
            ));
        }
        unix_fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        self.parent.sync_all()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for DestinationGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn open_absolute_directory(path: &Path) -> io::Result<std::fs::File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory is not absolute",
        ));
    }
    let descriptor = unix_fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let mut directory = std::fs::File::from(descriptor);
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory path contains traversal",
                ));
            }
        };
        let descriptor = unix_fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        directory = std::fs::File::from(descriptor);
    }
    Ok(directory)
}

fn create_entry_parent(
    artifact_directory: &std::fs::File,
    relative: &str,
) -> Result<(std::fs::File, OsString), MaterializeError> {
    let mut components = relative.split('/').peekable();
    let mut directory = artifact_directory.try_clone().map_err(|error| {
        MaterializeError::tampered_detail(
            "could not duplicate an artifact directory descriptor",
            error.to_string(),
        )
    })?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((directory, OsString::from(component)));
        }
        match unix_fs::mkdirat(&directory, component, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => {
                return Err(MaterializeError::tampered_detail(
                    "could not create a materialized artifact subdirectory",
                    error.to_string(),
                ));
            }
        }
        let descriptor = unix_fs::openat(
            &directory,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| {
            MaterializeError::tampered_detail(
                "could not open a materialized artifact subdirectory safely",
                error.to_string(),
            )
        })?;
        directory = std::fs::File::from(descriptor);
        unix_fs::fchmod(&directory, Mode::from_raw_mode(0o700)).map_err(|error| {
            MaterializeError::tampered_detail(
                "could not set materialized artifact directory permissions",
                error.to_string(),
            )
        })?;
    }
    Err(MaterializeError::tampered(
        "retained artifact file path is empty",
    ))
}

fn remove_contents(directory: &std::fs::File) -> io::Result<()> {
    let mut names = Vec::new();
    let mut entries = Dir::read_from(directory).map_err(io::Error::from)?;
    for entry in &mut entries {
        let entry = entry.map_err(io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(CString::new(name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "directory entry contains NUL")
            })?);
        }
    }
    for name in names {
        match unix_fs::openat(
            directory,
            name.as_c_str(),
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let child = std::fs::File::from(descriptor);
                remove_contents(&child)?;
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::REMOVEDIR)
                    .map_err(io::Error::from)?;
            }
            Err(_) => {
                unix_fs::unlinkat(directory, name.as_c_str(), AtFlags::empty())
                    .map_err(io::Error::from)?;
            }
        }
    }
    directory.sync_all()
}

fn file_identity(file: &std::fs::File) -> io::Result<(u64, u64)> {
    let stat = unix_fs::fstat(file).map_err(io::Error::from)?;
    Ok((stat.st_dev as _, stat.st_ino))
}

fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("encoded length is not a multiple of four".to_owned());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, quartet) in bytes.chunks_exact(4).enumerate() {
        let final_quartet = index + 1 == bytes.len() / 4;
        let first =
            base64_value(quartet[0]).ok_or_else(|| "invalid first base64 character".to_owned())?;
        let second =
            base64_value(quartet[1]).ok_or_else(|| "invalid second base64 character".to_owned())?;
        decoded.push((first << 2) | (second >> 4));
        if quartet[2] == b'=' {
            if !final_quartet || quartet[3] != b'=' || second & 0x0f != 0 {
                return Err("invalid canonical base64 padding".to_owned());
            }
            continue;
        }
        let third =
            base64_value(quartet[2]).ok_or_else(|| "invalid third base64 character".to_owned())?;
        decoded.push((second << 4) | (third >> 2));
        if quartet[3] == b'=' {
            if !final_quartet || third & 0x03 != 0 {
                return Err("invalid canonical base64 padding".to_owned());
            }
            continue;
        }
        let fourth =
            base64_value(quartet[3]).ok_or_else(|| "invalid fourth base64 character".to_owned())?;
        decoded.push((third << 6) | fourth);
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct ScriptedCall {
        responses: Mutex<VecDeque<(&'static str, Result<ResponseEnvelope, ProtocolError>)>>,
    }

    impl ScriptedCall {
        fn new(responses: Vec<(&'static str, Result<ResponseEnvelope, ProtocolError>)>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }

        fn exhausted(&self) -> bool {
            self.responses.lock().expect("responses").is_empty()
        }
    }

    impl V2Call for ScriptedCall {
        fn call<'a>(&'a self, operation: &'static str, _params: Value) -> V2CallFuture<'a> {
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected {operation} call"));
            assert_eq!(response.0, operation);
            Box::pin(std::future::ready(response.1))
        }
    }

    fn success<T: Serialize>(value: T) -> Result<ResponseEnvelope, ProtocolError> {
        ResponseEnvelope::success("fixture", value)
    }

    fn digest(bytes: &[u8]) -> String {
        lower_hex(&Sha256::digest(bytes))
    }

    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut result = String::new();
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            result.push(TABLE[(first >> 2) as usize] as char);
            result.push(TABLE[(((first & 3) << 4) | (second >> 4)) as usize] as char);
            result.push(if chunk.len() > 1 {
                TABLE[(((second & 15) << 2) | (third >> 6)) as usize] as char
            } else {
                '='
            });
            result.push(if chunk.len() > 2 {
                TABLE[(third & 63) as usize] as char
            } else {
                '='
            });
        }
        result
    }

    fn entry(path: &str, bytes: &[u8]) -> ArtifactEntry {
        ArtifactEntry {
            path: path.into(),
            size: bytes.len() as u64,
            sha256: digest(bytes),
        }
    }

    fn summary(name: &str, entries: &[ArtifactEntry]) -> ArtifactSummary {
        ArtifactSummary {
            name: name.into(),
            size: entries.iter().map(|entry| entry.size).sum(),
            files: entries.len() as u32,
            sha256: tree_digest(entries),
        }
    }

    fn root(summaries: Vec<ArtifactSummary>) -> ArtifactCatalog {
        ArtifactCatalog {
            repository_id: "r1111111111111111".into(),
            worktree_id: "w1111111111111111".into(),
            run_id: "t20260903T120000Z-abcd".into(),
            check: "browser".into(),
            manifest_sha256: "a".repeat(64),
            test: "complete".into(),
            requested_tier: ValidationTier::Release,
            readiness_eligible: true,
            proof: ProofKind::Complete,
            source_sha256: "b".repeat(64),
            config_sha256: "c".repeat(64),
            run_status: "passed".into(),
            run_complete: true,
            run_finished_at_epoch_ms: Some(10),
            run_metadata_sha256: "d".repeat(64),
            artifacts: summaries,
            artifact: None,
            entries: Vec::new(),
            next_offset: None,
        }
    }

    fn page(
        root: &ArtifactCatalog,
        summary: &ArtifactSummary,
        entries: Vec<ArtifactEntry>,
    ) -> ArtifactCatalog {
        let mut page = root.clone();
        page.artifact = Some(summary.clone());
        page.entries = entries;
        page
    }

    fn chunk(
        root: &ArtifactCatalog,
        summary: &ArtifactSummary,
        entry: &ArtifactEntry,
        offset: u64,
        bytes: &[u8],
        next_offset: Option<u64>,
    ) -> ArtifactChunk {
        ArtifactChunk {
            run_id: root.run_id.clone(),
            check: root.check.clone(),
            artifact: summary.name.clone(),
            file: entry.path.clone(),
            sha256: entry.sha256.clone(),
            total_bytes: entry.size,
            offset,
            bytes: bytes.len() as u32,
            base64: base64(bytes),
            next_offset,
        }
    }

    fn request(destination: PathBuf, artifacts: Vec<String>) -> MaterializeRequest {
        MaterializeRequest {
            path: PathBuf::from("/repository"),
            run_id: "t20260903T120000Z-abcd".into(),
            check: "browser".into(),
            artifacts,
            destination,
        }
    }

    #[tokio::test]
    async fn materializes_all_or_a_subset_with_exact_private_files() {
        let temporary = tempdir().expect("tempdir");
        let alpha_bytes = b"alpha report\n";
        let beta_bytes = b"beta image\0bytes";
        let alpha_entry = entry("nested/report.txt", alpha_bytes);
        let beta_entry = entry("image.bin", beta_bytes);
        let alpha = summary("alpha", std::slice::from_ref(&alpha_entry));
        let beta = summary("beta", std::slice::from_ref(&beta_entry));
        let catalog = root(vec![alpha.clone(), beta.clone()]);
        let caller = ScriptedCall::new(vec![
            ("test.artifact.catalog", success(catalog.clone())),
            (
                "test.artifact.catalog",
                success(page(&catalog, &alpha, vec![alpha_entry.clone()])),
            ),
            (
                "test.artifact.file",
                success(chunk(&catalog, &alpha, &alpha_entry, 0, alpha_bytes, None)),
            ),
            (
                "test.artifact.catalog",
                success(page(&catalog, &beta, vec![beta_entry.clone()])),
            ),
            (
                "test.artifact.file",
                success(chunk(&catalog, &beta, &beta_entry, 0, beta_bytes, None)),
            ),
        ]);
        let destination = temporary.path().join("materialized");
        let receipt = materialize(request(destination.clone(), Vec::new()), &caller)
            .await
            .expect("materialize");
        assert!(caller.exhausted());
        assert_eq!(receipt.artifacts.len(), 2);
        assert_eq!(
            std::fs::read(destination.join("alpha/nested/report.txt")).unwrap(),
            alpha_bytes
        );
        assert_eq!(
            std::fs::read(destination.join("beta/image.bin")).unwrap(),
            beta_bytes
        );
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(destination.join("beta/image.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let subset_destination = temporary.path().join("subset");
        let subset = ScriptedCall::new(vec![
            ("test.artifact.catalog", success(catalog.clone())),
            (
                "test.artifact.catalog",
                success(page(&catalog, &beta, vec![beta_entry.clone()])),
            ),
            (
                "test.artifact.file",
                success(chunk(&catalog, &beta, &beta_entry, 0, beta_bytes, None)),
            ),
        ]);
        let receipt = materialize(
            request(subset_destination.clone(), vec!["beta".into()]),
            &subset,
        )
        .await
        .expect("subset");
        assert_eq!(receipt.artifacts[0].name, "beta");
        assert!(!subset_destination.join("alpha").exists());
    }

    #[tokio::test]
    async fn pages_more_than_one_hundred_files_and_continues_large_chunks() {
        let temporary = tempdir().expect("tempdir");
        let entries = (0..101)
            .map(|index| entry(&format!("{index:03}.txt"), &[index as u8]))
            .collect::<Vec<_>>();
        let artifact = summary("many", &entries);
        let catalog = root(vec![artifact.clone()]);
        let mut first_page = page(&catalog, &artifact, entries[..100].to_vec());
        first_page.next_offset = Some(100);
        let second_page = page(&catalog, &artifact, entries[100..].to_vec());
        let mut responses = vec![
            ("test.artifact.catalog", success(catalog.clone())),
            ("test.artifact.catalog", success(first_page)),
            ("test.artifact.catalog", success(second_page)),
        ];
        for (index, entry) in entries.iter().enumerate() {
            responses.push((
                "test.artifact.file",
                success(chunk(&catalog, &artifact, entry, 0, &[index as u8], None)),
            ));
        }
        let caller = ScriptedCall::new(responses);
        let destination = temporary.path().join("paged");
        materialize(request(destination.clone(), Vec::new()), &caller)
            .await
            .expect("paged materialization");
        assert_eq!(
            std::fs::read(destination.join("many/100.txt")).unwrap(),
            [100]
        );

        let large = vec![7_u8; MAX_CHUNK_BYTES as usize + 17];
        let large_entry = entry("large.bin", &large);
        let large_artifact = summary("large", std::slice::from_ref(&large_entry));
        let large_root = root(vec![large_artifact.clone()]);
        let split = MAX_CHUNK_BYTES as usize;
        let caller = ScriptedCall::new(vec![
            ("test.artifact.catalog", success(large_root.clone())),
            (
                "test.artifact.catalog",
                success(page(
                    &large_root,
                    &large_artifact,
                    vec![large_entry.clone()],
                )),
            ),
            (
                "test.artifact.file",
                success(chunk(
                    &large_root,
                    &large_artifact,
                    &large_entry,
                    0,
                    &large[..split],
                    Some(split as u64),
                )),
            ),
            (
                "test.artifact.file",
                success(chunk(
                    &large_root,
                    &large_artifact,
                    &large_entry,
                    split as u64,
                    &large[split..],
                    None,
                )),
            ),
        ]);
        let destination = temporary.path().join("large");
        materialize(request(destination.clone(), Vec::new()), &caller)
            .await
            .expect("large materialization");
        assert_eq!(
            std::fs::read(destination.join("large/large.bin")).unwrap(),
            large
        );
    }

    #[tokio::test]
    async fn tamper_or_transport_failure_removes_only_the_new_destination() {
        let temporary = tempdir().expect("tempdir");
        let sentinel = temporary.path().join("sentinel");
        std::fs::write(&sentinel, "preserve").unwrap();
        let bytes = b"truth";
        let file_entry = entry("file.txt", bytes);
        let artifact = summary("proof", std::slice::from_ref(&file_entry));
        let catalog = root(vec![artifact.clone()]);
        let mut bad_chunk = chunk(&catalog, &artifact, &file_entry, 0, bytes, None);
        bad_chunk.base64 = "!!!!".into();
        let destination = temporary.path().join("failed");
        let caller = ScriptedCall::new(vec![
            ("test.artifact.catalog", success(catalog.clone())),
            (
                "test.artifact.catalog",
                success(page(&catalog, &artifact, vec![file_entry.clone()])),
            ),
            ("test.artifact.file", success(bad_chunk)),
        ]);
        let error = materialize(request(destination.clone(), Vec::new()), &caller)
            .await
            .expect_err("tamper");
        assert_eq!(error.classification, ExitClassification::Operation);
        assert_eq!(error.code, ErrorCode::TestArtifactTampered);
        assert!(!destination.exists());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "preserve");

        let transport_destination = temporary.path().join("transport");
        let caller = ScriptedCall::new(vec![(
            "test.artifact.catalog",
            Err(ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "unavailable",
            )),
        )]);
        let error = materialize(request(transport_destination.clone(), Vec::new()), &caller)
            .await
            .expect_err("transport");
        assert_eq!(error.exit_code(), 2);
        assert!(!transport_destination.exists());

        let existing = temporary.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::write(existing.join("owned"), "keep").unwrap();
        let caller = ScriptedCall::new(Vec::new());
        let error = materialize(request(existing.clone(), Vec::new()), &caller)
            .await
            .expect_err("preexisting");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(
            std::fs::read_to_string(existing.join("owned")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn traversal_symlink_and_canonical_base64_guards_are_strict() {
        assert!(!valid_relative_file("../escape"));
        assert!(!valid_relative_file("nested\\escape"));
        assert!(decode_base64("YQ==").is_ok());
        assert!(decode_base64("YR==").is_err());
        assert!(decode_base64("YQ=A").is_err());
        let temporary = tempdir().expect("tempdir");
        let real = temporary.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = temporary.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(DestinationPlan::prepare(&linked.join("new")).is_err());
    }
}
