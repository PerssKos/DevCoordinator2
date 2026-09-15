//! Check the index (or an explicit committed tree), never unstaged substitutes.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{Value, json};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const ALLOWLIST: &str = ".devcoordinator-commit-allowlist.json";

struct Entry {
    mode: String,
    object: String,
    path: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowlist {
    version: u8,
    large_files: Vec<Exception>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exception {
    path: String,
    blob: String,
    reason: String,
}

fn git(root: &Path, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("Git could not read the requested index or tree".into());
    }
    Ok(output.stdout)
}

fn inventory(root: &Path, tree: Option<&str>) -> Result<BTreeMap<String, Entry>, String> {
    let output = if let Some(tree) = tree {
        let resolved = git(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{tree}^{{tree}}"),
            ],
        )?;
        let resolved = std::str::from_utf8(&resolved)
            .map_err(|_| "invalid Git tree identity")?
            .trim();
        git(root, &["ls-tree", "-r", "-z", resolved])?
    } else {
        git(root, &["ls-files", "--stage", "-z"])?
    };
    let mut entries = BTreeMap::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let text = std::str::from_utf8(record).map_err(|_| "index paths must be UTF-8")?;
        let (metadata, path) = text.split_once('\t').ok_or("invalid Git index entry")?;
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err("invalid Git index metadata".into());
        }
        let (object, stage) = if tree.is_some() {
            (fields[2], "0")
        } else {
            (fields[1], fields[2])
        };
        if stage != "0" {
            return Err("resolve staged merge conflicts before committing".into());
        }
        if !(object.len() == 40 || object.len() == 64)
            || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid staged object identity".into());
        }
        if !Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err("staged path is not repository-relative".into());
        }
        entries.insert(
            path.into(),
            Entry {
                mode: fields[0].into(),
                object: object.into(),
                path: path.into(),
            },
        );
    }
    Ok(entries)
}

struct Objects {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Objects {
    fn new(root: &Path) -> Result<Self, String> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["cat-file", "--batch-command"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let input = child.stdin.take().ok_or("missing Git input")?;
        let output = BufReader::new(child.stdout.take().ok_or("missing Git output")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }

    fn read(&mut self, object: &str) -> Result<Vec<u8>, String> {
        writeln!(self.input, "contents {object}")
            .and_then(|()| self.input.flush())
            .map_err(|error| error.to_string())?;
        let mut header = String::new();
        self.output
            .read_line(&mut header)
            .map_err(|error| error.to_string())?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || fields[0] != object || fields[1] != "blob" {
            return Err("staged object is not a blob".into());
        }
        let size = fields[2]
            .parse::<usize>()
            .map_err(|_| "invalid staged object size")?;
        let mut content = vec![0; size];
        self.output
            .read_exact(&mut content)
            .map_err(|error| error.to_string())?;
        let mut newline = [0];
        self.output
            .read_exact(&mut newline)
            .map_err(|error| error.to_string())?;
        if newline != [b'\n'] {
            return Err("invalid staged object framing".into());
        }
        Ok(content)
    }

    fn size(&mut self, object: &str) -> Result<u64, String> {
        writeln!(self.input, "info {object}")
            .and_then(|()| self.input.flush())
            .map_err(|error| error.to_string())?;
        let mut header = String::new();
        self.output
            .read_line(&mut header)
            .map_err(|error| error.to_string())?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || fields[0] != object || fields[1] != "blob" {
            return Err("staged object is not a blob".into());
        }
        fields[2]
            .parse()
            .map_err(|_| "invalid staged object size".into())
    }
}

impl Drop for Objects {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn private_path(path: &str) -> bool {
    path.split('/').any(|part| {
        matches!(
            part,
            ".git"
                | ".devcoordinator"
                | ".codex-artifacts"
                | "node_modules"
                | "playwright-report"
                | "test-results"
        )
    }) || path.starts_with("target/")
        || path.starts_with("instance/")
        || path.starts_with(".local/")
        || matches!(
            Path::new(path).file_name().and_then(|name| name.to_str()),
            Some(
                ".env"
                    | "storage-state.json"
                    | "auth-state.json"
                    | "cookies.json"
                    | "usage.sqlite3"
            )
        )
}

fn executable(data: &[u8]) -> bool {
    data.starts_with(b"#!/")
        || data.starts_with(b"#! /")
        || data.starts_with(b"\x7fELF")
        || data.starts_with(b"MZ")
        || [
            b"\xcf\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xca\xfe\xba\xbe",
        ]
        .iter()
        .any(|magic| data.starts_with(*magic))
}

pub fn check(root: &Path, tree: Option<&str>) -> Result<Value, String> {
    let canonical = root.canonicalize().map_err(|error| error.to_string())?;
    let actual = git(root, &["rev-parse", "--show-toplevel"])?;
    let actual = Path::new(
        std::str::from_utf8(&actual)
            .map_err(|_| "invalid Git worktree path")?
            .trim(),
    )
    .canonicalize()
    .map_err(|error| error.to_string())?;
    if canonical != actual {
        return Err("pre-commit root must be the exact Git worktree root".into());
    }
    let entries = inventory(root, tree)?;
    let mut selected = if tree.is_some() {
        entries.keys().cloned().collect::<BTreeSet<_>>()
    } else {
        git(
            root,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--diff-filter=ACMR",
                "-z",
            ],
        )?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).map_err(|_| "staged paths must be UTF-8".to_owned())
        })
        .collect::<Result<BTreeSet<_>, _>>()?
    };
    for path in selected.clone() {
        if path.ends_with(".png") {
            selected.insert(format!("{path}.provenance.json"));
        }
        if let Some(image) = path
            .strip_suffix(".provenance.json")
            .filter(|path| path.ends_with(".png"))
        {
            selected.insert(image.into());
        }
    }
    let mut objects = Objects::new(root)?;
    let allowlist = if let Some(entry) = entries.get(ALLOWLIST) {
        if entry.mode != "100644" {
            return Err("commit allowlist must be a regular non-executable file".into());
        }
        if objects.size(&entry.object)? > 65536 {
            return Err("staged commit allowlist exceeds 64 KiB".into());
        }
        let list: Allowlist = serde_json::from_slice(&objects.read(&entry.object)?)
            .map_err(|_| "staged commit allowlist is invalid")?;
        if list.version != 1
            || list
                .large_files
                .iter()
                .any(|entry| entry.reason.trim().len() < 8)
        {
            return Err("commit exceptions require version one and a review reason".into());
        }
        list
    } else {
        Allowlist::default()
    };
    let snapshot = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut findings = Vec::new();
    let mut checked = 0;
    for path in &selected {
        let Some(entry) = entries.get(path) else {
            continue;
        };
        checked += 1;
        if private_path(path) {
            findings.push(json!({"path":path,"rule":"private-or-generated-evidence"}));
            continue;
        }
        if entry.mode == "160000" {
            continue;
        }
        if entry.mode == "120000" {
            findings.push(json!({"path":path,"rule":"staged-symlink"}));
            continue;
        }
        let size = objects.size(&entry.object)?;
        if size > MAX_FILE_BYTES
            && !allowlist
                .large_files
                .iter()
                .any(|exception| exception.path == *path && exception.blob == entry.object)
        {
            findings.push(
                json!({"path":path,"rule":"large-staged-file","bytes":size,"limit":MAX_FILE_BYTES}),
            );
            continue;
        }
        let data = objects.read(&entry.object)?;
        if entry.mode == "100755" && !executable(&data)
            || path == ".githooks/pre-commit" && entry.mode != "100755"
        {
            findings.push(json!({"path":path,"rule":"executable-mode"}));
        }
        let destination = snapshot.path().join(&entry.path);
        fs::create_dir_all(destination.parent().unwrap()).map_err(|error| error.to_string())?;
        fs::write(destination, data).map_err(|error| error.to_string())?;
    }
    let public = crate::public_artifacts::scan(snapshot.path(), false)?;
    findings.extend(public["findings"].as_array().cloned().unwrap_or_default());
    let total = findings.len();
    findings.truncate(50);
    Ok(
        json!({"ok":total==0,"scope":if tree.is_some(){"committed_tree"}else{"staged_index"},"checked":checked,"findings":findings,"total_findings":total,"findings_truncated":total>50}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]).unwrap();
        directory
    }

    fn has(report: &Value, rule: &str) -> bool {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["rule"] == rule)
    }

    #[test]
    fn staged_secret_cannot_be_hidden_by_an_unstaged_fix() {
        let directory = repository();
        let root = directory.path();
        let secret = format!("ghp_{}", "A".repeat(36));
        fs::write(
            root.join("config.txt"),
            format!("Authorization: Bearer {secret}\n"),
        )
        .unwrap();
        git(root, &["add", "config.txt"]).unwrap();
        fs::write(root.join("config.txt"), "safe unstaged content\n").unwrap();
        let before = git(root, &["ls-files", "--stage"]).unwrap();
        let report = check(root, None).unwrap();
        assert!(has(&report, "text-secret"));
        assert!(!report.to_string().contains(&secret));
        assert_eq!(before, git(root, &["ls-files", "--stage"]).unwrap());
        assert_eq!(
            fs::read_to_string(root.join("config.txt")).unwrap(),
            "safe unstaged content\n"
        );
        git(root, &["add", "config.txt"]).unwrap();
        assert_eq!(check(root, None).unwrap()["ok"], true);
    }

    #[test]
    fn modes_generated_evidence_and_links_are_checked_together() {
        let directory = repository();
        let root = directory.path();
        fs::write(root.join("README.md"), "ordinary prose").unwrap();
        fs::set_permissions(root.join("README.md"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/report.json"), "{}").unwrap();
        symlink("/etc/passwd", root.join("linked.txt")).unwrap();
        fs::write(root.join("fixture.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        git(root, &["add", "README.md", "linked.txt", "fixture.sh"]).unwrap();
        git(root, &["add", "-f", "target/report.json"]).unwrap();
        let report = check(root, None).unwrap();
        assert!(has(&report, "executable-mode"));
        assert!(has(&report, "private-or-generated-evidence"));
        assert!(has(&report, "staged-symlink"));
        assert!(
            !report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["path"] == "fixture.sh")
        );
    }

    #[test]
    fn large_file_exceptions_are_bound_to_the_staged_blob() {
        let directory = repository();
        let root = directory.path();
        fs::File::create(root.join("large.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        git(root, &["add", "large.bin"]).unwrap();
        assert!(has(&check(root, None).unwrap(), "large-staged-file"));
        let object = String::from_utf8(git(root, &["rev-parse", ":large.bin"]).unwrap()).unwrap();
        fs::write(root.join(ALLOWLIST), json!({"version":1,"large_files":[{"path":"large.bin","blob":object.trim(),"reason":"Reviewed synthetic sparse fixture"}]}).to_string()).unwrap();
        assert!(has(&check(root, None).unwrap(), "large-staged-file"));
        git(root, &["add", ALLOWLIST]).unwrap();
        assert_eq!(check(root, None).unwrap()["ok"], true);
        use std::io::Seek;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(root.join("large.bin"))
            .unwrap();
        file.seek(std::io::SeekFrom::Start(MAX_FILE_BYTES)).unwrap();
        file.write_all(b"x").unwrap();
        git(root, &["add", "large.bin"]).unwrap();
        assert!(has(&check(root, None).unwrap(), "large-staged-file"));
    }

    #[test]
    fn committed_tree_is_checked_even_when_the_index_has_no_changes() {
        let directory = repository();
        let root = directory.path();
        fs::create_dir(root.join(".devcoordinator")).unwrap();
        fs::write(root.join(".devcoordinator/evidence.json"), "{}").unwrap();
        git(root, &["add", "-f", ".devcoordinator/evidence.json"]).unwrap();
        git(
            root,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
        assert_eq!(check(root, None).unwrap()["checked"], 0);
        assert!(has(
            &check(root, Some("HEAD")).unwrap(),
            "private-or-generated-evidence"
        ));
    }
}
