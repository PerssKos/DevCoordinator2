use super::{CheckError, CheckErrorKind, MAX_TEXT_BYTES};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MARKER: &str = "<!-- codex:focused-policy:v1 -->";
const CORE_TITLE: &str = "# Universal Agent Instructions — Mandatory Core";
const AUDIT_TITLE: &str = "# Universal Agent Instructions";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 32 * 1024;
const MAX_BUNDLE_BYTES: usize = 120 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct PolicyBundle {
    pub(super) contract_text: String,
    pub(super) sources: Vec<PolicySource>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct PolicySource {
    pub(super) path: PathBuf,
    pub(super) scan_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    modules: Vec<Module>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Module {
    id: String,
    relative_path: String,
    applicability: Vec<String>,
}

pub(super) fn read_policy_bundle(path: &Path) -> Result<PolicyBundle, CheckError> {
    let source = path.canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputUnavailable,
            "policy source is unavailable",
        )
    })?;
    let core = read_text(&source, MAX_TEXT_BYTES)?;
    if !core.starts_with("<!-- codex:focused-policy:") {
        return Ok(PolicyBundle {
            contract_text: core.clone(),
            sources: vec![PolicySource {
                path: path.to_path_buf(),
                scan_text: core,
            }],
        });
    }
    if core.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(invalid("focused policy core exceeds its byte limit"));
    }
    let (marker, remainder) = core
        .split_once('\n')
        .ok_or_else(|| invalid("invalid focused policy header"))?;
    if marker.trim_end_matches('\r') != MARKER {
        return Err(invalid("unsupported focused policy version"));
    }
    let (title, body) = remainder.split_once('\n').unwrap_or((remainder, ""));
    if title.trim_end_matches('\r') != CORE_TITLE {
        return Err(invalid("invalid focused policy title"));
    }
    let root = source
        .parent()
        .ok_or_else(|| invalid("policy source has no directory"))?;
    let manifest_path = confined_path(root, "modules.json")?;
    let manifest: Manifest = serde_json::from_str(&read_text(&manifest_path, MAX_MANIFEST_BYTES)?)
        .map_err(|_| invalid("invalid focused policy manifest"))?;
    if manifest.version != 1 || manifest.modules.is_empty() || manifest.modules.len() > 32 {
        return Err(invalid("unsupported or oversized focused policy manifest"));
    }
    let mut bundle = PolicyBundle {
        contract_text: format!("{AUDIT_TITLE}\n{body}"),
        sources: vec![PolicySource {
            path: path.to_path_buf(),
            scan_text: format!("\n{remainder}"),
        }],
    };
    let mut total_bytes = core.len();
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for module in manifest.modules {
        if !valid_token(&module.id)
            || !ids.insert(module.id)
            || !paths.insert(module.relative_path.clone())
            || !module.relative_path.starts_with("modules/")
            || module.applicability.is_empty()
            || module.applicability.len() > 16
            || module.applicability.iter().any(|tag| !valid_token(tag))
        {
            return Err(invalid("invalid focused policy module entry"));
        }
        let module_path = confined_path(root, &module.relative_path)?;
        let text = read_text(&module_path, MAX_DOCUMENT_BYTES)?;
        if text.trim().is_empty() {
            return Err(invalid("focused policy module is empty"));
        }
        total_bytes += text.len() + 2;
        if total_bytes > MAX_BUNDLE_BYTES {
            return Err(invalid("focused policy bundle exceeds its byte limit"));
        }
        bundle.contract_text.push_str("\n\n");
        bundle.contract_text.push_str(&text);
        bundle.sources.push(PolicySource {
            path: module_path,
            scan_text: text,
        });
    }
    Ok(bundle)
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn confined_path(root: &Path, relative: &str) -> Result<PathBuf, CheckError> {
    if relative.is_empty()
        || relative.len() > 256
        || relative.contains(['\\', ':', '\0'])
        || relative
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(invalid("invalid focused policy relative path"));
    }
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            CheckError::new(
                CheckErrorKind::InputUnavailable,
                "policy input is unavailable",
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("policy inputs must not traverse symlinks"));
        }
    }
    Ok(path)
}

fn read_text(path: &Path, limit: u64) -> Result<String, CheckError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputUnavailable,
            "input file is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("input must be a regular non-symlinked file"));
    }
    if metadata.len() > limit {
        return Err(invalid("input file is too large"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|error| {
            CheckError::new(
                if error.kind() == io::ErrorKind::InvalidData {
                    CheckErrorKind::InvalidInput
                } else {
                    CheckErrorKind::InputUnavailable
                },
                "input file could not be read",
            )
        })?;
    if bytes.len() as u64 > limit {
        return Err(invalid("input file is too large"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("input must be valid UTF-8"))
}

fn invalid(message: &str) -> CheckError {
    CheckError::new(CheckErrorKind::InvalidInput, message)
}

#[cfg(test)]
#[path = "policy_bundle_tests.rs"]
mod tests;
