//! Publish bounded diagnostics from isolated CI skill runs, never their fixtures.
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};

use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
use serde_json::{Value, json};

use crate::audit_ledger::{create_directory_all_nofollow, write_new_bytes_nofollow};

const TAIL_BYTES: u64 = 16 * 1024;
const MAX_FILES: usize = 1024;

fn read_tail(root: &Path, path: &Path) -> Result<(String, bool), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "diagnostic escaped run root")?;
    let mut descriptor = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| error.to_string())?;
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("invalid diagnostic path".into());
        };
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if index + 1 < components.len() {
            flags |= OFlags::DIRECTORY;
        }
        descriptor =
            openat(&descriptor, *name, flags, Mode::empty()).map_err(|error| error.to_string())?;
    }
    let stat = fstat(&descriptor).map_err(|error| error.to_string())?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err("diagnostic is not a regular file".into());
    }
    let size = u64::try_from(stat.st_size).map_err(|_| "invalid diagnostic size")?;
    let mut file = File::from(descriptor);
    let start = size.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Ok((sanitize(text), start > 0))
}

fn sanitize(text: &str) -> String {
    text.lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.contains("authorization")
                || lower.contains("cookie")
                || lower.contains("public-artifact-guard: allow")
                || !crate::public_artifacts::scan_text(Path::new("ci-diagnostic.txt"), line)
                    .is_empty()
            {
                "[private diagnostic line withheld]".to_owned()
            } else {
                line.chars()
                    .filter(|character| !character.is_control() || *character == '\t')
                    .take(2000)
                    .collect()
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn collect(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut Vec<Value>,
) -> Result<(), String> {
    if depth > 8 || entries.len() >= MAX_FILES {
        return Err("CI diagnostic inventory exceeds its bound".into());
    }
    let mut children = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let kind = child.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        let path = child.path();
        let descend = match depth {
            0 => child.file_name() == "checks",
            1 => true,
            2 => matches!(
                child.file_name().to_str(),
                Some("check" | "discovery" | "cases")
            ),
            3 => directory.file_name().is_some_and(|name| name == "cases"),
            _ => false,
        };
        if kind.is_dir() && descend {
            collect(root, &path, depth + 1, entries)?;
        } else if kind.is_file()
            && matches!(
                child.file_name().to_str(),
                Some("stdout.log" | "stderr.log")
            )
        {
            let (text, truncated) = read_tail(root, &path)?;
            entries.push(json!({"stream":path.strip_prefix(root).unwrap().to_string_lossy(),"tail":text,"truncated":truncated}));
        }
    }
    Ok(())
}

pub fn export(root: &Path, output: &Path) -> Result<Value, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let runs = root.join(".devcoordinator/agent-validation");
    create_directory_all_nofollow(output, 0o700).map_err(|error| error.to_string())?;
    let mut count = 0;
    if runs.exists() {
        crate::audit_ledger::validate_directory_nofollow(&runs)
            .map_err(|error| error.to_string())?;
        for entry in fs::read_dir(&runs).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
                || !name.starts_with("skills-")
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                continue;
            }
            if count >= 100 {
                return Err("too many CI validation runs".into());
            }
            let run = entry.path();
            let mut diagnostics = Vec::new();
            let logs = run.join("logs/runs").join(&name);
            if logs.is_dir() {
                crate::audit_ledger::validate_directory_nofollow(&logs)
                    .map_err(|error| error.to_string())?;
                collect(&run, &logs, 0, &mut diagnostics)?;
            }
            for stream in ["executor.stdout.log", "executor.stderr.log"] {
                let path = run.join(stream);
                if path.symlink_metadata().is_ok() {
                    let (text, truncated) = read_tail(&run, &path)?;
                    diagnostics.push(json!({"stream":stream,"tail":text,"truncated":truncated}));
                }
            }
            let report_path = run.join("check-report.json");
            let report = if report_path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 2 * 1024 * 1024)
            {
                crate::audit_ledger::read_bytes_nofollow(&report_path, Some(&root))
                    .map_err(|error| error.to_string())?
            } else {
                None
            };
            let report = report.as_deref().and_then(|bytes| {
                devcoordinator2_executor_protocol::ExecutionReport::from_json(bytes).ok()
            });
            let summary = report.map(|report| json!({"status":report.status,"counts":report.counts,"failure_index":report.failure_index,"source_digest":report.source_digest,"config_digest":report.config_digest}));
            let document = json!({"schema":1,"run_id":name,"report_available":summary.is_some(),"summary":summary,"diagnostics":diagnostics});
            let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
            write_new_bytes_nofollow(&output.join(format!("{name}.json")), &bytes, 0o600)
                .map_err(|error| error.to_string())?;
            count += 1;
        }
    }
    let manifest = json!({"schema":1,"runs":count,"scope":"isolated CI validation diagnostics","tail_bytes_per_stream":TAIL_BYTES});
    write_new_bytes_nofollow(
        &output.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest).unwrap(),
        0o600,
    )
    .map_err(|error| error.to_string())?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn failed_executor_diagnostics_survive_without_a_final_report_or_fixture_export() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let run = root.join(".devcoordinator/agent-validation/skills-fixture");
        let leaf = run.join("logs/runs/skills-fixture/checks/compiler/check");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(
            leaf.join("stderr.log"),
            "error[E0308]: mismatched types\nassertion failed: count == 2\n",
        )
        .unwrap();
        fs::write(
            run.join("executor.stderr.log"),
            "executor exited before sealing report",
        )
        .unwrap();
        fs::write(run.join("auth-state.json"), "never export this fixture").unwrap();
        fs::write(root.join("private.txt"), "never export outside content").unwrap();
        symlink(root.join("private.txt"), leaf.join("stdout.log")).unwrap();
        let output = root.join("export");
        assert_eq!(export(root, &output).unwrap()["runs"], 1);
        let content = fs::read_to_string(output.join("skills-fixture.json")).unwrap();
        assert!(content.contains("error[E0308]"));
        assert!(content.contains("executor exited before sealing report"));
        assert!(!content.contains("never export"));
        assert!(content.contains("\"report_available\": false"));
    }

    #[test]
    fn tails_are_bounded_and_authentication_and_private_lines_are_withheld() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stderr.log");
        fs::write(
            &path,
            format!("{}\nlast diagnostic", "long output\n".repeat(10000)),
        )
        .unwrap();
        let (tail, truncated) = read_tail(temporary.path(), &path).unwrap();
        assert!(truncated && tail.len() <= TAIL_BYTES as usize);
        assert!(tail.ends_with("last diagnostic"));
        let secret = format!("ghp_{}", "A".repeat(36));
        let cleaned = sanitize(&format!(
            "Authorization: Bearer {secret}\nSet-Cookie: secret\nTOKEN={secret}\nerror: expected a value"
        ));
        assert!(!cleaned.contains(&secret));
        assert!(!cleaned.contains("Set-Cookie"));
        assert!(cleaned.contains("error: expected a value"));
    }
}
