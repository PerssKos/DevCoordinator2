//! Reusable contracts shared by the repository-audit verifiers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::audit_findings::{canonical_json_sha256, strict_json_object};
use crate::audit_ledger::{read_bytes_nofollow, validate_directory_nofollow};

pub const INTERACTION_CHECKLIST_LABELS: [&str; 10] = [
    "badge-detail",
    "row-hit-target",
    "navigation-cursor",
    "transient-disclosure",
    "disclosure-scrollbar",
    "icon-meaning",
    "stable-expansion-width",
    "hover-copy",
    "status-summary",
    "message-metadata",
];

static SECTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^##\s+(.+?)\s*$").expect("constant section regex"));
static SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:?-{3,}:?$").expect("constant separator regex"));

pub fn interaction_checklist_missing(text: &str) -> Vec<String> {
    INTERACTION_CHECKLIST_LABELS
        .iter()
        .filter(|label| {
            let pattern = Regex::new(&format!(
                r"(?i){}\s*(?:[=:]|\|)\s*(?:pass|passed|gap|blocked|not[\s\-]?applicable|n/?a)",
                regex::escape(label)
            ))
            .expect("escaped checklist regex");
            !pattern.is_match(text)
        })
        .map(|label| (*label).to_owned())
        .collect()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        })
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("file not found: {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

pub fn iter_report_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut reports = Vec::new();
    for path in paths {
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let directory = validate_directory_nofollow(path).map_err(|error| error.to_string())?;
            let mut children = std::fs::read_dir(directory)
                .map_err(|error| format!("cannot list {}: {error}", path.display()))?
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    entry
                        .file_type()
                        .ok()
                        .filter(|kind| kind.is_file())
                        .and_then(|_| {
                            (entry.path().extension().and_then(|value| value.to_str())
                                == Some("md"))
                            .then_some(entry.path())
                        })
                })
                .collect::<Vec<_>>();
            children.sort();
            reports.extend(children);
        } else if metadata.is_file() {
            reports.push(path.clone());
        }
    }
    Ok(reports)
}

pub fn duplicate_values(values: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            duplicates.insert(value.clone());
        }
    }
    duplicates.into_iter().collect()
}

pub fn load_json_object(path: &Path, name: &str) -> Result<Map<String, Value>, String> {
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{name} does not exist: {}", path.display()))?;
    strict_json_object(&bytes, name)
        .map_err(|_| format!("{name} is not valid JSON: {}", path.display()))
}

pub fn load_json_list(path: &Path, name: &str) -> Result<Vec<Value>, String> {
    let bytes = read_bytes_nofollow(path, None)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{name} does not exist: {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("{name} is not valid JSON: {}", path.display()))?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| format!("{name} must be a JSON list."))
}

pub fn canonical_value_sha256(value: &Value) -> Result<String, String> {
    canonical_json_sha256(value)
}

pub fn section_bodies(text: &str) -> BTreeMap<String, String> {
    let mut bodies: Vec<(String, Vec<String>)> = Vec::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(captures) = SECTION_RE.captures(line.trim()) {
            let key = captures[1].trim().to_lowercase();
            current = bodies.iter().position(|(existing, _)| existing == &key);
            if current.is_none() {
                bodies.push((key, Vec::new()));
                current = Some(bodies.len() - 1);
            }
        } else if let Some(index) = current {
            bodies[index].1.push(line.to_owned());
        }
    }
    bodies
        .into_iter()
        .map(|(key, lines)| (key, lines.join("\n").trim().to_owned()))
        .collect()
}

pub fn section_order(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| SECTION_RE.captures(line.trim()))
        .map(|captures| captures[1].trim().to_lowercase())
        .collect()
}

pub fn split_markdown_row(row: &str) -> Vec<String> {
    let stripped = row.trim();
    if !stripped.starts_with('|') || !stripped.ends_with('|') {
        return Vec::new();
    }
    let body = &stripped[1..stripped.len() - 1];
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in body.chars() {
        if escaped {
            if character == '|' {
                current.push('|');
            } else {
                current.push('\\');
                current.push(character);
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '|' {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    cells.push(current.trim().to_owned());
    cells
}

pub fn is_separator_row(columns: &[String]) -> bool {
    !columns.is_empty()
        && columns
            .iter()
            .all(|column| SEPARATOR_RE.is_match(column.trim()))
}

pub fn parse_markdown_table_dicts(text: &str) -> Vec<BTreeMap<String, String>> {
    let rows = text
        .lines()
        .map(split_markdown_row)
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    if rows.len() < 2 {
        return Vec::new();
    }
    let mut parsed = Vec::new();
    let mut index = 0;
    while index < rows.len() - 1 {
        let header = &rows[index];
        let separator = &rows[index + 1];
        if is_separator_row(separator) {
            let mut cursor = index + 2;
            while cursor < rows.len()
                && rows[cursor].len() == header.len()
                && !is_separator_row(&rows[cursor])
            {
                parsed.push(
                    header
                        .iter()
                        .zip(&rows[cursor])
                        .map(|(key, value)| (key.to_lowercase(), value.clone()))
                        .collect(),
                );
                cursor += 1;
            }
            index = cursor;
        } else {
            index += 1;
        }
    }
    parsed
}

pub fn declared_values(text: &str, heading: &str) -> Vec<String> {
    let bodies = section_bodies(text);
    bodies
        .get(&heading.to_lowercase())
        .map(|body| {
            body.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| line.trim_matches('`').to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_checklist_accepts_inline_and_table_statuses() {
        let text = INTERACTION_CHECKLIST_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| {
                if index % 2 == 0 {
                    format!("{label}=pass")
                } else {
                    format!("| {label} | not-applicable |")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(interaction_checklist_missing(&text).is_empty());
        assert_eq!(
            interaction_checklist_missing("badge-detail=passed"),
            INTERACTION_CHECKLIST_LABELS[1..]
        );
    }

    #[test]
    fn tables_sections_and_declared_values_match_legacy_contract() {
        let text =
            "## Files\n`a.rs`\n`b.rs`\n\n| Name | Detail |\n| --- | :---: |\n| first | A \\| B |\n";
        assert_eq!(section_order(text), ["files"]);
        assert_eq!(
            declared_values(text, "FILES"),
            [
                "a.rs",
                "b.rs",
                "| Name | Detail |",
                "| --- | :---: |",
                "| first | A \\| B |"
            ]
        );
        let rows = parse_markdown_table_dicts(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["detail"], "A | B");
    }

    #[test]
    fn duplicate_and_report_helpers_are_deterministic_and_nofollow() {
        assert_eq!(
            duplicate_values(&["b".to_owned(), "a".to_owned(), "b".to_owned()]),
            ["b"]
        );
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("b.md"), "b").unwrap();
        std::fs::write(directory.path().join("a.md"), "a").unwrap();
        std::fs::write(directory.path().join("ignored.txt"), "x").unwrap();
        assert_eq!(
            iter_report_files(&[directory.path().to_owned()]).unwrap(),
            [directory.path().join("a.md"), directory.path().join("b.md")]
        );
        let link = directory.path().join("linked.md");
        std::os::unix::fs::symlink(directory.path().join("a.md"), &link).unwrap();
        assert!(sha256_file(&link).is_err());
    }
}
