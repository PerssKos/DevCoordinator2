//! Exact, private receipts for verifier self-qualification, never rendered readiness.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub type Inputs = BTreeMap<String, String>;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    inputs: Inputs,
    summary: Value,
    summary_sha256: String,
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file(path: &Path) -> Result<String, String> {
    let mut input = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let before = input.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() {
        return Err("qualification input is not a regular file".into());
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let after = input.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("qualification input changed during hashing".into());
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn tree(root: &Path) -> Result<String, String> {
    crate::audit_ledger::validate_directory_nofollow(root).map_err(|e| e.to_string())?;
    let canonical = root.canonicalize().map_err(|e| e.to_string())?;
    let mut pending = vec![root.to_path_buf()];
    let mut files = BTreeMap::new();
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            visited += 1;
            if visited > 16384 {
                return Err("qualification tree exceeds its file bound".into());
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned(),
                    file(&entry.path())?,
                );
            } else if kind.is_symlink() {
                let target = entry.path().canonicalize().map_err(|e| e.to_string())?;
                if !target.starts_with(&canonical) {
                    return Err("qualification link escapes its input tree".into());
                }
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned(),
                    format!(
                        "link:{}",
                        fs::read_link(entry.path())
                            .map_err(|e| e.to_string())?
                            .display()
                    ),
                );
            } else {
                return Err("qualification tree contains an unverified file type".into());
            }
            if files.len() + pending.len() > 16384 {
                return Err("qualification tree exceeds its file bound".into());
            }
        }
    }
    Ok(hash(
        &serde_json::to_vec(&files).map_err(|e| e.to_string())?,
    ))
}

fn node() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("FORMAL_WEB_UI_NODE") {
        let path = PathBuf::from(configured);
        if path.components().count() > 1 || path.is_absolute() {
            return path.canonicalize().ok();
        }
        return std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|part| part.join(&path))
            .find(|path| path.is_file())?
            .canonicalize()
            .ok();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|part| part.join("node"))
        .find(|path| path.is_file())?
        .canonicalize()
        .ok()
}

pub fn inputs(
    root: &Path,
    modules: &Path,
    fixture: Option<&Path>,
    timeout: u64,
) -> Result<Inputs, String> {
    let node = node().ok_or("qualification Node runtime is unavailable")?;
    let output = Command::new(&node)
        .args([
            "-e",
            "const path=require('path');const root=process.argv[1];let registry;try{registry=require(path.join(root,'playwright-core/lib/coreBundle.js')).registry.registry;}catch{registry=require(path.join(root,'playwright-core/lib/server/registry/index.js')).registry;}process.stdout.write(JSON.stringify(['chromium','chromium-headless-shell'].map(name=>registry.findExecutable(name).directory)));",
        ])
        .arg(modules)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() || output.stdout.len() > 4096 {
        return Err("qualification browser identity is unavailable".into());
    }
    let browsers: Vec<PathBuf> =
        serde_json::from_slice(&output.stdout).map_err(|_| "invalid browser identity")?;
    if browsers.len() != 2 {
        return Err("both managed browser distributions must be bound".into());
    }
    let browser_hashes = browsers
        .iter()
        .map(|path| tree(path))
        .collect::<Result<Vec<_>, _>>()?;
    let platform = Command::new("uname")
        .args(["-srm"])
        .output()
        .map_err(|e| e.to_string())?;
    if !platform.status.success() {
        return Err("platform identity unavailable".into());
    }
    let mut result = Inputs::from([
        (
            "verifier".into(),
            tree(&root.join("skills/formal-web-ui-verification"))?,
        ),
        (
            "fixtures".into(),
            file(&root.join("rust/tooling/src/formal_selftest.rs"))?,
        ),
        (
            "tooling".into(),
            file(&std::env::current_exe().map_err(|e| e.to_string())?)?,
        ),
        ("node".into(), file(&node)?),
        (
            "browser".into(),
            hash(&serde_json::to_vec(&browser_hashes).map_err(|e| e.to_string())?),
        ),
        ("playwright".into(), tree(&modules.join("playwright"))?),
        (
            "playwright-core".into(),
            tree(&modules.join("playwright-core"))?,
        ),
        (
            "lockfile".into(),
            file(&root.join("ci/playwright/package-lock.json"))?,
        ),
        ("platform".into(), hash(&platform.stdout)),
        (
            "contract".into(),
            hash(
                &serde_json::to_vec(&devcoordinator2_api::contract_document())
                    .map_err(|e| e.to_string())?,
            ),
        ),
        ("timeout".into(), timeout.to_string()),
    ]);
    // Bind only stable execution inputs. Shell bookkeeping (PWD, SHLVL, CI
    // run IDs, timestamps and temporary paths) must not make an unchanged
    // verifier miss its receipt; paths and binaries above already bind the
    // interpreter, browser, lockfile and fixture dependencies.
    let environment = std::env::vars_os()
        .filter(|(name, _)| {
            matches!(
                name.to_str(),
                Some(
                    "PATH"
                        | "LANG"
                        | "LC_ALL"
                        | "LC_CTYPE"
                        | "TZ"
                        | "NODE_PATH"
                        | "NODE_OPTIONS"
                        | "NODE_EXTRA_CA_CERTS"
                        | "SSL_CERT_FILE"
                        | "SSL_CERT_DIR"
                        | "LD_LIBRARY_PATH"
                        | "DYLD_LIBRARY_PATH"
                        | "FONTCONFIG_PATH"
                        | "FORMAL_WEB_UI_NODE"
                        | "FORMAL_WEB_UI_PLAYWRIGHT_NODE_MODULES"
                )
            )
        })
        .map(|(name, value)| {
            (
                name.as_encoded_bytes().to_vec(),
                value.as_encoded_bytes().to_vec(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    result.insert(
        "environment".into(),
        hash(
            &serde_json::to_vec(&environment.into_iter().collect::<Vec<_>>())
                .map_err(|e| e.to_string())?,
        ),
    );
    if let Some(fixture) = fixture {
        result.insert("coordinator-fixture".into(), file(fixture)?);
    } else {
        return Err(
            "exact coordinator fixture identity is required for qualification reuse".into(),
        );
    }
    Ok(result)
}

fn valid_summary(value: &Value) -> bool {
    value["ok"] == true
        && value["suite"] == "formal-web-ui-verification"
        && value["phase"] == "all"
        && value["qualification_browser"] == "playwright-managed-browser"
        && value["static_cases"]
            .as_u64()
            .is_some_and(|count| count > 0)
}

/// Only bound managed browsers may establish a reusable qualification receipt.
/// A normal uncached self-test can still use the verifier's reviewed fallback.
pub fn managed_browser_evidence(root: &Path) -> bool {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0;
    let mut observed = false;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            visited += 1;
            if visited > 16384 {
                return false;
            }
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                let Ok(metadata) = entry.metadata() else {
                    return false;
                };
                if metadata.len() > 64 * 1024 * 1024 {
                    return false;
                }
                let Ok(bytes) = fs::read(entry.path()) else {
                    return false;
                };
                if let Ok(report) = serde_json::from_slice::<Value>(&bytes)
                    && let Some(browser) = report.get("browser").and_then(Value::as_str)
                {
                    if browser != "playwright-managed-browser" {
                        return false;
                    }
                    observed = true;
                }
            }
        }
    }
    observed
}

pub fn lookup(directory: &Path, inputs: &Inputs) -> Option<Value> {
    crate::audit_ledger::validate_directory_nofollow(directory).ok()?;
    let path = directory.join("formal-qualification.json");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return None;
    }
    if metadata.len() > 65536 {
        return None;
    }
    let mut input = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .ok()?;
    let mut bytes = Vec::new();
    input.by_ref().take(65537).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 65536 {
        return None;
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).ok()?;
    if receipt.version != 1
        || &receipt.inputs != inputs
        || !valid_summary(&receipt.summary)
        || receipt.summary_sha256 != hash(&serde_json::to_vec(&receipt.summary).ok()?)
    {
        return None;
    }
    let mut summary = receipt.summary;
    summary["qualification"] = json!("reused");
    summary["qualification_inputs_sha256"] = json!(hash(&serde_json::to_vec(inputs).ok()?));
    Some(summary)
}

pub fn record(directory: &Path, inputs: &Inputs, summary: &Value) -> Result<(), String> {
    if !valid_summary(summary) {
        return Err("only a complete passing qualification can be cached".into());
    }
    crate::audit_ledger::create_directory_all_nofollow(directory, 0o700)
        .map_err(|e| e.to_string())?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let mut summary = summary.clone();
    summary["report"] = Value::Null;
    summary["workspace"] = Value::Null;
    summary
        .as_object_mut()
        .ok_or("invalid qualification summary")?
        .remove("qualification");
    let receipt = Receipt {
        version: 1,
        inputs: inputs.clone(),
        summary_sha256: hash(&serde_json::to_vec(&summary).map_err(|e| e.to_string())?),
        summary,
    };
    let bytes = serde_json::to_vec(&receipt).map_err(|e| e.to_string())?;
    let temporary = directory.join(format!("formal-qualification-{}.tmp", std::process::id()));
    crate::audit_ledger::write_bytes_nofollow(&temporary, &bytes, 0o600)
        .map_err(|e| e.to_string())?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let outcome = fs::rename(&temporary, directory.join("formal-qualification.json"))
        .map_err(|e| e.to_string());
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_bound_input_and_corrupt_receipt_invalidates_qualification() {
        let directory = tempfile::tempdir().unwrap();
        let inputs = Inputs::from(
            [
                "verifier",
                "fixtures",
                "browser",
                "dependencies",
                "platform",
                "contract",
            ]
            .map(|name| (name.into(), "old".into())),
        );
        let summary = json!({"ok":true,"suite":"formal-web-ui-verification","phase":"all","static_cases":3,"report":"removed-report.json","qualification_browser":"playwright-managed-browser"});
        record(directory.path(), &inputs, &summary).unwrap();
        let hit = lookup(directory.path(), &inputs).unwrap();
        assert_eq!(hit["qualification"], "reused");
        assert!(hit["report"].is_null());
        for key in inputs.keys() {
            let mut changed = inputs.clone();
            changed.insert(key.clone(), "new".into());
            assert!(lookup(directory.path(), &changed).is_none(), "{key}");
        }
        fs::write(
            directory.path().join("formal-qualification.json"),
            b"corrupt",
        )
        .unwrap();
        assert!(lookup(directory.path(), &inputs).is_none());
        assert!(record(directory.path(), &inputs, &json!({"ok":false})).is_err());
    }
    #[test]
    fn replacement_browser_bytes_change_the_tree_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("browser"), b"one").unwrap();
        let before = tree(directory.path()).unwrap();
        fs::write(directory.path().join("browser"), b"two").unwrap();
        assert_ne!(before, tree(directory.path()).unwrap());
    }

    #[test]
    fn fallback_browser_and_missing_rendered_proof_are_never_cacheable() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!managed_browser_evidence(directory.path()));
        let report = directory.path().join("report.json");
        fs::write(&report, br#"{"browser":"playwright-managed-browser"}"#).unwrap();
        assert!(managed_browser_evidence(directory.path()));
        fs::write(&report, br#"{"browser":"/usr/bin/chromium"}"#).unwrap();
        assert!(!managed_browser_evidence(directory.path()));
    }
}
