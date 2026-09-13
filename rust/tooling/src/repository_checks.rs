//! Portable repository-policy checks used by `devcoordinator2-tooling check`.
//!
//! Every public report is bounded and contains only rule identifiers, relative
//! paths, line numbers, and fixed explanatory text. Matched repository text,
//! forbidden instance strings, Git stderr, and private paths are never copied
//! into findings.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

mod policy_bundle;
use policy_bundle::read_policy_bundle;

pub const MAX_REPOSITORY_CHECK_FINDINGS: usize = 256;
const MAX_GIT_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CheckStatus {
    Clean,
    Findings,
}

impl CheckStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Findings => "findings",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CheckFinding {
    pub rule: String,
    pub path: String,
    pub line: Option<u64>,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckReport {
    pub check: &'static str,
    pub status: CheckStatus,
    pub total_findings: usize,
    pub findings_truncated: bool,
    pub findings: Vec<CheckFinding>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.status == CheckStatus::Clean
    }

    pub fn to_json(&self) -> Value {
        json!({
            "schema": 1,
            "check": self.check,
            "status": self.status.as_str(),
            "total_findings": self.total_findings,
            "findings_truncated": self.findings_truncated,
            "findings": self.findings.iter().map(|finding| json!({
                "rule": finding.rule,
                "path": finding.path,
                "line": finding.line,
                "detail": finding.detail,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckErrorKind {
    RepositoryUnavailable,
    InputUnavailable,
    InvalidInput,
    GitUnavailable,
    GitFailed,
    GitOutputInvalid,
    InputChanged,
}

impl CheckErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::InputUnavailable => "input_unavailable",
            Self::InvalidInput => "invalid_input",
            Self::GitUnavailable => "git_unavailable",
            Self::GitFailed => "git_failed",
            Self::GitOutputInvalid => "git_output_invalid",
            Self::InputChanged => "input_changed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckError {
    pub kind: CheckErrorKind,
    pub message: String,
}

impl CheckError {
    fn new(kind: CheckErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CheckError {}

#[derive(Default)]
struct FindingCollector {
    findings: Vec<CheckFinding>,
    total: usize,
}

impl FindingCollector {
    fn push(&mut self, finding: CheckFinding) {
        self.total += 1;
        if self.findings.len() < MAX_REPOSITORY_CHECK_FINDINGS {
            self.findings.push(finding);
        }
    }

    fn finish(mut self, check: &'static str) -> CheckReport {
        self.findings.sort();
        CheckReport {
            check,
            status: if self.total == 0 {
                CheckStatus::Clean
            } else {
                CheckStatus::Findings
            },
            total_findings: self.total,
            findings_truncated: self.total > self.findings.len(),
            findings: self.findings,
        }
    }
}

fn finding(rule: &str, path: &str, line: Option<usize>, detail: &str) -> CheckFinding {
    CheckFinding {
        rule: rule.to_owned(),
        path: bounded_string(path),
        line: line.and_then(|value| u64::try_from(value).ok()),
        detail: detail.to_owned(),
    }
}

fn bounded_string(value: &str) -> String {
    if value.len() <= MAX_PATH_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_PATH_BYTES.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value.to_string_lossy().into_owned(),
            Component::ParentDir => "..".to_owned(),
            Component::CurDir => ".".to_owned(),
            Component::RootDir => String::new(),
            Component::Prefix(value) => value.as_os_str().to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_directory(path: &Path) -> Result<PathBuf, CheckError> {
    let canonical = path.canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::RepositoryUnavailable,
            "repository directory is unavailable",
        )
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::RepositoryUnavailable,
            "repository directory is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CheckError::new(
            CheckErrorKind::RepositoryUnavailable,
            "repository must be a regular non-symlinked directory",
        ));
    }
    Ok(canonical)
}

fn command_output(mut command: Command) -> Result<Output, CheckError> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = command.output().map_err(|error| {
        let kind = if error.kind() == io::ErrorKind::NotFound {
            CheckErrorKind::GitUnavailable
        } else {
            CheckErrorKind::GitFailed
        };
        CheckError::new(kind, "could not execute Git")
    })?;
    if output.stdout.len() > MAX_GIT_OUTPUT_BYTES || output.stderr.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(CheckError::new(
            CheckErrorKind::GitOutputInvalid,
            "Git returned an oversized result",
        ));
    }
    Ok(output)
}

fn git_output(repo: &Path, args: &[&str], require_success: bool) -> Result<Output, CheckError> {
    let mut command = Command::new("git");
    command.current_dir(repo).args(args);
    let output = command_output(command)?;
    if require_success && !output.status.success() {
        return Err(CheckError::new(
            CheckErrorKind::GitFailed,
            format!(
                "Git operation failed with exit code {}",
                output.status.code().unwrap_or(-1)
            ),
        ));
    }
    Ok(output)
}

fn utf8_stdout(output: &Output) -> Result<&str, CheckError> {
    std::str::from_utf8(&output.stdout).map_err(|_| {
        CheckError::new(
            CheckErrorKind::GitOutputInvalid,
            "Git returned non-UTF-8 output",
        )
    })
}

fn read_bounded_utf8(path: &Path) -> Result<String, CheckError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputUnavailable,
            "input file is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "input must be a regular non-symlinked file",
        ));
    }
    if metadata.len() > MAX_TEXT_BYTES {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "input file is too large",
        ));
    }
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            CheckError::new(CheckErrorKind::InvalidInput, "input must be valid UTF-8")
        } else {
            CheckError::new(
                CheckErrorKind::InputUnavailable,
                "input file is unavailable",
            )
        }
    })
}

fn walk_tree(root: &Path) -> Result<Vec<PathBuf>, CheckError> {
    fn visit(root: &Path, current: &Path, result: &mut Vec<PathBuf>) -> Result<(), CheckError> {
        let entries = fs::read_dir(current).map_err(|_| {
            CheckError::new(
                CheckErrorKind::InputUnavailable,
                "could not traverse repository content",
            )
        })?;
        let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|_| {
            CheckError::new(
                CheckErrorKind::InputUnavailable,
                "could not traverse repository content",
            )
        })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|_| {
                CheckError::new(
                    CheckErrorKind::InvalidInput,
                    "repository traversal escaped its root",
                )
            })?;
            if relative.components().any(|component| {
                matches!(component, Component::Normal(value) if value == OsStr::new(".git") || value == OsStr::new("node_modules") || value == OsStr::new("__pycache__") || value == OsStr::new("target"))
            }) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| {
                CheckError::new(
                    CheckErrorKind::InputChanged,
                    "repository entry changed during traversal",
                )
            })?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(root, &path, result)?;
            } else if metadata.is_file() {
                result.push(relative.to_path_buf());
            }
        }
        Ok(())
    }

    let mut result = Vec::new();
    visit(root, root, &mut result)?;
    result.sort();
    Ok(result)
}

/// Scan every Git-committable text file for installation-specific strings.
pub fn check_no_instance_data(
    repository_root: &Path,
    patterns_file: &Path,
) -> Result<CheckReport, CheckError> {
    let root = canonical_directory(repository_root)?;
    let patterns_text = read_bounded_utf8(patterns_file)?;
    let patterns = patterns_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "instance-data pattern list is empty",
        ));
    }

    let output = git_output(
        &root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        true,
    )?;
    let names = utf8_stdout(&output)?;
    let mut collector = FindingCollector::default();
    for name in names.split('\0').filter(|name| !name.is_empty()) {
        let relative = Path::new(name);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            collector.push(finding(
                "unsafe-committable-path",
                name,
                None,
                "Git reported a path outside the repository",
            ));
            continue;
        }
        let path = root.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_TEXT_BYTES
        {
            continue;
        }
        let Ok(data) = fs::read(&path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(data) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let folded = line.to_lowercase();
            for pattern in &patterns {
                if folded.contains(pattern) {
                    collector.push(finding(
                        "instance-data",
                        &slash_path(relative),
                        Some(index + 1),
                        "contains a forbidden instance-specific pattern",
                    ));
                }
            }
        }
    }
    Ok(collector.finish("no_instance_data"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceLanguage {
    Python,
    JavaScript,
    Rust,
}

fn strip_comments_and_strings(text: &str, language: SourceLanguage) -> String {
    let bytes = text.as_bytes();
    let mut output = bytes.to_vec();
    let mut index = 0usize;
    let mut quote = None;
    let mut triple = false;
    let mut escaped = false;
    let mut block_comment = false;
    while index < bytes.len() {
        if block_comment {
            output[index] = if bytes[index] == b'\n' { b'\n' } else { b' ' };
            if index + 1 < bytes.len() && bytes[index] == b'*' && bytes[index + 1] == b'/' {
                output[index + 1] = b' ';
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            output[index] = if bytes[index] == b'\n' { b'\n' } else { b' ' };
            if escaped {
                escaped = false;
                index += 1;
                continue;
            }
            if bytes[index] == b'\\' {
                escaped = true;
                index += 1;
                continue;
            }
            if triple
                && index + 2 < bytes.len()
                && bytes[index] == delimiter
                && bytes[index + 1] == delimiter
                && bytes[index + 2] == delimiter
            {
                output[index + 1] = b' ';
                output[index + 2] = b' ';
                quote = None;
                triple = false;
                index += 3;
            } else if !triple && bytes[index] == delimiter {
                quote = None;
                index += 1;
            } else {
                index += 1;
            }
            continue;
        }
        if language != SourceLanguage::Python
            && index + 1 < bytes.len()
            && bytes[index] == b'/'
            && bytes[index + 1] == b'*'
        {
            output[index] = b' ';
            output[index + 1] = b' ';
            block_comment = true;
            index += 2;
            continue;
        }
        let line_comment = match language {
            SourceLanguage::Python => bytes[index] == b'#',
            SourceLanguage::JavaScript | SourceLanguage::Rust => {
                index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'/'
            }
        };
        if line_comment {
            while index < bytes.len() && bytes[index] != b'\n' {
                output[index] = b' ';
                index += 1;
            }
            continue;
        }
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            let delimiter = bytes[index];
            let python_triple = language == SourceLanguage::Python
                && delimiter != b'`'
                && index + 2 < bytes.len()
                && bytes[index + 1] == delimiter
                && bytes[index + 2] == delimiter;
            output[index] = b' ';
            if python_triple {
                output[index + 1] = b' ';
                output[index + 2] = b' ';
                index += 3;
            } else {
                index += 1;
            }
            quote = Some(delimiter);
            triple = python_triple;
            continue;
        }
        index += 1;
    }
    String::from_utf8(output).expect("input began as UTF-8")
}

fn line_at(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn first_call_argument(text: &str, call_end: usize) -> Option<&str> {
    let tail = &text[call_end..];
    let mut depth = 0usize;
    for (index, character) in tail.char_indices() {
        match character {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(tail[..index].trim()),
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(tail[..index].trim()),
            _ => {}
        }
    }
    None
}

fn parse_first_numeric_argument(text: &str, call_end: usize) -> Option<f64> {
    let mut argument = first_call_argument(text, call_end)?;
    if argument.starts_with(['-', '+']) {
        return None;
    }
    while argument.starts_with('(') && argument.ends_with(')') {
        argument = argument[1..argument.len() - 1].trim();
    }
    argument.parse::<f64>().ok()
}

fn duration_millis(expression: &str) -> Option<f64> {
    let compact = expression
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    for (constructor, multiplier) in [
        ("Duration::from_millis(", 1.0),
        ("std::time::Duration::from_millis(", 1.0),
        ("StdDuration::from_millis(", 1.0),
        ("Duration::from_secs_f64(", 1_000.0),
        ("std::time::Duration::from_secs_f64(", 1_000.0),
        ("StdDuration::from_secs_f64(", 1_000.0),
    ] {
        if let Some(value) = compact
            .strip_prefix(constructor)
            .and_then(|value| value.strip_suffix(')'))
            .and_then(|value| value.parse::<f64>().ok())
        {
            return Some(value * multiplier);
        }
    }
    None
}

fn declared_duration_millis(text: &str, name: &str) -> Option<f64> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let declaration = format!("const {name}:");
    text.lines().find_map(|line| {
        if line.trim_start().starts_with(&declaration) {
            line.split_once('=')
                .and_then(|(_, value)| value.trim().strip_suffix(';'))
                .and_then(duration_millis)
        } else {
            None
        }
    })
}

fn rust_wait_is_bounded(text: &str, call_end: usize) -> bool {
    let Some(argument) = first_call_argument(text, call_end) else {
        return false;
    };
    if duration_millis(argument).is_some_and(|millis| millis <= 100.0) {
        return true;
    }
    let compact = argument
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let name = compact
        .split_once(".min(")
        .map_or(compact.as_str(), |(name, _)| name);
    declared_duration_millis(text, name).is_some_and(|millis| millis <= 100.0)
}

/// Inspect one governed source without returning matched source text.
pub fn scan_timer_waits_in_text(
    text: &str,
    relative_path: &str,
    language: SourceLanguage,
) -> Vec<CheckFinding> {
    let clean = if language == SourceLanguage::JavaScript {
        text.to_owned()
    } else {
        strip_comments_and_strings(text, language)
    };
    let markers: &[&str] = match language {
        SourceLanguage::Python => &["time.sleep(", "asyncio.sleep("],
        SourceLanguage::JavaScript => &["waitForTimeout(", "setTimeout("],
        SourceLanguage::Rust => &[
            "thread::sleep(",
            "std::thread::sleep(",
            "tokio::time::sleep(",
        ],
    };
    let mut findings = Vec::new();
    for marker in markers {
        let mut offset = 0usize;
        while let Some(relative) = clean[offset..].find(marker) {
            let found = offset + relative;
            let end = found + marker.len();
            let allowed = match language {
                SourceLanguage::Python => {
                    parse_first_numeric_argument(&clean, end).is_some_and(|seconds| seconds <= 0.1)
                }
                SourceLanguage::JavaScript => false,
                SourceLanguage::Rust => rust_wait_is_bounded(&clean, end),
            };
            if !allowed {
                findings.push(finding(
                    "clock-based-wait",
                    relative_path,
                    Some(line_at(&clean, found)),
                    "clock-based sleep is not completion evidence",
                ));
            }
            offset = end;
        }
    }
    findings.sort();
    findings
}

/// Reject clock-based progression in governed test and verification code.
pub fn check_no_test_timer_waits(root: &Path) -> Result<CheckReport, CheckError> {
    let root = canonical_directory(root)?;
    let legacy_python_targets = [
        "src/devcoordinator2/daemon/test_admission.py",
        "src/devcoordinator2/daemon/test_postgres.py",
        "src/devcoordinator2/daemon/tests_lifecycle.py",
        "src/devcoordinator2/daemon/systemd_unit.py",
        "src/devcoordinator2/daemon/metrics_sampler.py",
        "scripts/install.py",
        "tests/integration/helpers.py",
    ];
    let mut targets = BTreeMap::<PathBuf, SourceLanguage>::new();
    for relative in legacy_python_targets {
        let path = PathBuf::from(relative);
        if root.join(&path).is_file() {
            targets.insert(path, SourceLanguage::Python);
        }
    }
    let integration = root.join("tests/integration");
    if integration.is_dir() {
        for relative in walk_tree(&integration)? {
            let name = relative
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default();
            if name.starts_with("test_") && name.ends_with(".py") {
                targets.insert(
                    PathBuf::from("tests/integration").join(relative),
                    SourceLanguage::Python,
                );
            }
        }
    }
    let console = PathBuf::from("console/verify.mjs");
    if root.join(&console).is_file() {
        targets.insert(console, SourceLanguage::JavaScript);
    }
    for directory in ["rust/control/src", "rust/control/tests"] {
        let absolute = root.join(directory);
        if absolute.is_dir() {
            for relative in walk_tree(&absolute)? {
                if relative.extension() == Some(OsStr::new("rs")) {
                    targets.insert(
                        PathBuf::from(directory).join(relative),
                        SourceLanguage::Rust,
                    );
                }
            }
        }
    }

    let mut collector = FindingCollector::default();
    for (relative, language) in targets {
        let text = read_bounded_utf8(&root.join(&relative))?;
        for item in scan_timer_waits_in_text(&text, &slash_path(&relative), language) {
            collector.push(item);
        }
    }
    Ok(collector.finish("no_test_timer_waits"))
}

fn ascii_word_boundary(text: &str, start: usize, length: usize) -> bool {
    let bytes = text.as_bytes();
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    (start == 0 || !word(bytes[start - 1]))
        && (start + length == bytes.len() || !word(bytes[start + length]))
}

fn contains_word_case_insensitive(text: &str, needle: &str) -> bool {
    let folded = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut offset = 0usize;
    while let Some(relative) = folded[offset..].find(&needle) {
        let start = offset + relative;
        if ascii_word_boundary(&folded, start, needle.len()) {
            return true;
        }
        offset = start + needle.len();
    }
    false
}

fn neutrality_paths(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), CheckError> {
    let mut markup = vec![
        PathBuf::from("reference/universal/AGENTS.md"),
        PathBuf::from("SKILL_AUDIT.md"),
    ];
    let skills = root.join("skills");
    if skills.is_dir() {
        let mut directories = fs::read_dir(&skills)
            .map_err(|_| CheckError::new(CheckErrorKind::InputUnavailable, "cannot read skills"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| CheckError::new(CheckErrorKind::InputUnavailable, "cannot read skills"))?;
        directories.sort_by_key(|entry| entry.file_name());
        for entry in directories {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| {
                CheckError::new(
                    CheckErrorKind::InputChanged,
                    "skill entry changed during traversal",
                )
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let prefix = PathBuf::from("skills").join(entry.file_name());
            for name in ["SKILL.md", "README.md"] {
                let relative = prefix.join(name);
                if root.join(&relative).is_file() {
                    markup.push(relative);
                }
            }
            for directory in ["references", "agents"] {
                let absolute = root.join(&prefix).join(directory);
                if absolute.is_dir() {
                    for child in walk_tree(&absolute)? {
                        let include = if directory == "references" {
                            child.extension() == Some(OsStr::new("md"))
                        } else {
                            child
                                .parent()
                                .is_some_and(|parent| parent.as_os_str().is_empty())
                                && child.extension() == Some(OsStr::new("yaml"))
                        };
                        if include {
                            markup.push(prefix.join(directory).join(child));
                        }
                    }
                }
            }
        }
    }
    markup.sort();
    markup.dedup();

    let mut prompts = Vec::new();
    for relative in [
        "rust/tooling/src/audit_queue.rs",
        "rust/tooling/src/test_coverage_audit.rs",
        "rust/tooling/src/ui_audit.rs",
        "rust/tooling/src/journey_docs.rs",
    ] {
        let path = PathBuf::from(relative);
        if root.join(&path).is_file() {
            prompts.push(path);
        }
    }
    prompts.sort();
    Ok((markup, prompts))
}

fn scan_neutrality_file(collector: &mut FindingCollector, text: &str, path: &str, prompt: bool) {
    for (index, line) in text.lines().enumerate() {
        let folded = line.to_ascii_lowercase();
        let runtime_name = if prompt {
            contains_word_case_insensitive(line, "in Codex")
                || contains_word_case_insensitive(line, "Codex worker")
                || contains_word_case_insensitive(line, "Codex workers")
                || contains_word_case_insensitive(line, "Codex skills")
                || contains_word_case_insensitive(line, "Claude Code")
        } else {
            contains_word_case_insensitive(line, "Codex")
                || contains_word_case_insensitive(line, "Claude")
                || contains_word_case_insensitive(line, "Claude Code")
        };
        if runtime_name {
            collector.push(finding(
                "runtime-name",
                path,
                Some(index + 1),
                "shared contract is runtime-specific",
            ));
        }
        if !prompt
            && (folded.contains(".codex/")
                || folded.contains(".codex\\")
                || folded.contains(".claude/")
                || folded.contains(".claude\\")
                || contains_word_case_insensitive(line, ".codex")
                || contains_word_case_insensitive(line, ".claude"))
        {
            collector.push(finding(
                "runtime-home",
                path,
                Some(index + 1),
                "shared contract is runtime-specific",
            ));
        }
        if ["fork_turns", "request_user_input", "AskUserQuestion"]
            .iter()
            .any(|needle| line.contains(needle))
        {
            collector.push(finding(
                "runtime-api",
                path,
                Some(index + 1),
                "shared contract is runtime-specific",
            ));
        }
    }
}

/// Reject runtime-specific assumptions from shared contracts and prompts.
pub fn audit_agent_neutrality(root: &Path) -> Result<CheckReport, CheckError> {
    let root = canonical_directory(root)?;
    let (markup, prompts) = neutrality_paths(&root)?;
    let mut collector = FindingCollector::default();
    for relative in markup {
        let path = root.join(&relative);
        if !path.is_file() {
            continue;
        }
        if relative == Path::new("reference/universal/AGENTS.md") {
            for source in read_policy_bundle(&path)?.sources {
                let source_path = source.path.strip_prefix(&root).map_err(|_| {
                    CheckError::new(
                        CheckErrorKind::InvalidInput,
                        "policy source is outside the repository",
                    )
                })?;
                scan_neutrality_file(
                    &mut collector,
                    &source.scan_text,
                    &slash_path(source_path),
                    false,
                );
            }
        } else {
            let text = read_bounded_utf8(&path)?;
            scan_neutrality_file(&mut collector, &text, &slash_path(&relative), false);
        }
    }
    for relative in prompts {
        let text = read_bounded_utf8(&root.join(&relative))?;
        scan_neutrality_file(&mut collector, &text, &slash_path(&relative), true);
    }
    Ok(collector.finish("agent_neutrality"))
}

fn yaml_without_comments(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            line.split('#')
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn workflow_has_pull_request_trigger(lines: &[String]) -> bool {
    for (index, line) in lines.iter().enumerate() {
        let Some(inline) = line.strip_prefix("on:") else {
            continue;
        };
        if contains_word_case_insensitive(inline, "pull_request")
            || contains_word_case_insensitive(inline, "pull_request_target")
        {
            return true;
        }
        for nested in &lines[index + 1..] {
            if !nested.is_empty() && !nested.starts_with(' ') {
                break;
            }
            let trimmed = nested.trim_start();
            if trimmed.starts_with("pull_request:") || trimmed.starts_with("pull_request_target:") {
                return true;
            }
        }
        return false;
    }
    false
}

fn valid_yaml_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn workflow_job_blocks(lines: &[String]) -> Result<BTreeMap<String, Vec<String>>, CheckError> {
    let Some(jobs_index) = lines.iter().position(|line| line == "jobs:") else {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "workflow has no top-level jobs section",
        ));
    };
    let mut jobs = BTreeMap::<String, Vec<String>>::new();
    let mut current: Option<String> = None;
    for line in &lines[jobs_index + 1..] {
        if !line.is_empty()
            && !line.starts_with(' ')
            && line
                .strip_suffix(':')
                .is_some_and(|name| valid_yaml_name(name.trim()))
        {
            break;
        }
        if line.starts_with("  ")
            && !line.starts_with("    ")
            && let Some(name) = line
                .strip_prefix("  ")
                .and_then(|value| value.strip_suffix(':'))
            && valid_yaml_name(name)
        {
            current = Some(name.to_owned());
            jobs.entry(name.to_owned()).or_default();
            continue;
        }
        if let Some(name) = &current {
            jobs.get_mut(name)
                .expect("job was inserted")
                .push(line.clone());
        }
    }
    if jobs.is_empty() {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "workflow jobs section is empty or malformed",
        ));
    }
    Ok(jobs)
}

fn strip_yaml_quotes(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn hosted_runner(value: &str) -> bool {
    let value = strip_yaml_quotes(value);
    let Some((family, version)) = value.rsplit_once('-') else {
        return false;
    };
    match family {
        "ubuntu" => {
            version == "latest"
                || (!version.is_empty()
                    && version.split('.').all(|part| {
                        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                    }))
        }
        "windows" | "macos" => {
            version == "latest"
                || (!version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit()))
        }
        _ => false,
    }
}

fn matrix_axis_values(lines: &[String], axis: &str) -> Option<Vec<String>> {
    let prefix = format!("        {axis}:");
    for line in lines {
        if let Some(inline) = line.strip_prefix(&prefix) {
            let inner = inline.trim().strip_prefix('[')?.strip_suffix(']')?;
            return Some(
                inner
                    .split(',')
                    .map(strip_yaml_quotes)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect(),
            );
        }
    }
    None
}

fn matrix_expression(value: &str) -> Option<&str> {
    let compact = value.trim();
    let inner = compact.strip_prefix("${{")?.strip_suffix("}}")?.trim();
    inner
        .strip_prefix("matrix.")
        .filter(|axis| valid_yaml_name(axis))
}

fn job_runs_self_hosted(lines: &[String]) -> bool {
    for (index, line) in lines.iter().enumerate() {
        let Some(inline) = line.strip_prefix("    runs-on:") else {
            continue;
        };
        let inline = inline.trim();
        if !inline.is_empty() {
            if let Some(axis) = matrix_expression(inline) {
                return matrix_axis_values(lines, axis)
                    .is_none_or(|values| values.iter().any(|value| !hosted_runner(value)));
            }
            if let Some(inner) = inline
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
            {
                let values = inner
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>();
                return values.len() != 1 || !hosted_runner(values[0]);
            }
            return !hosted_runner(inline);
        }
        let mut values = Vec::new();
        for nested in &lines[index + 1..] {
            let indent = nested.len() - nested.trim_start_matches(' ').len();
            if !nested.is_empty() && indent <= 4 {
                break;
            }
            if let Some(value) = nested.trim_start().strip_prefix("- ") {
                values.push(value.trim());
            }
        }
        return values.len() != 1 || !hosted_runner(values[0]);
    }
    false
}

fn job_condition(lines: &[String]) -> &str {
    lines
        .iter()
        .find_map(|line| line.strip_prefix("    if:"))
        .map(str::trim)
        .unwrap_or_default()
}

fn trusted_job_condition(condition: &str) -> bool {
    let compact = condition
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let Some(body) = compact
        .strip_prefix("${{")
        .and_then(|value| value.strip_suffix("}}"))
    else {
        return false;
    };
    let body = body
        .replace("\"push\"", "'push'")
        .replace("\"workflow_dispatch\"", "'workflow_dispatch'");
    let forward = "github.event_name=='push'||github.event_name=='workflow_dispatch'";
    let reverse = "github.event_name=='workflow_dispatch'||github.event_name=='push'";
    let allowed = |value: &str| value == forward || value == reverse;
    if allowed(&body)
        || body
            .strip_prefix('(')
            .and_then(|value| value.strip_suffix(')'))
            .is_some_and(allowed)
    {
        return true;
    }
    let Some((gate, events)) = body.split_once("&&(") else {
        return false;
    };
    let Some(events) = events.strip_suffix(')') else {
        return false;
    };
    let Some((variable, value)) = gate.split_once("=='") else {
        return false;
    };
    let Some(value) = value.strip_suffix('\'') else {
        return false;
    };
    let variable = variable.strip_prefix("vars.").unwrap_or_default();
    let mut variable_bytes = variable.bytes();
    variable_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && variable_bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        && allowed(events)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CiSecurityResult {
    pub report: CheckReport,
    pub self_hosted_jobs: usize,
    pub pull_request: bool,
}

impl CiSecurityResult {
    pub fn to_json(&self) -> Value {
        let mut report = self.report.to_json();
        if let Some(object) = report.as_object_mut() {
            object.insert("self_hosted_jobs".into(), self.self_hosted_jobs.into());
            object.insert("pull_request".into(), self.pull_request.into());
        }
        report
    }
}

/// Inspect the repository's intentionally simple workflow shape.
pub fn find_ci_security_violations(text: &str) -> Result<CiSecurityResult, CheckError> {
    let lines = yaml_without_comments(text);
    let jobs = workflow_job_blocks(&lines)?;
    let pull_request = workflow_has_pull_request_trigger(&lines);
    let self_hosted = jobs
        .iter()
        .filter(|(_, lines)| job_runs_self_hosted(lines))
        .collect::<Vec<_>>();
    let mut collector = FindingCollector::default();
    if pull_request {
        for (name, lines) in &self_hosted {
            if !trusted_job_condition(job_condition(lines)) {
                collector.push(finding(
                    "self-hosted-pull-request",
                    ".github/workflows/validate.yml",
                    None,
                    &format!(
                        "self-hosted job '{name}' must have a job-level event allowlist for only push and workflow_dispatch while pull_request is enabled"
                    ),
                ));
            }
        }
    }
    Ok(CiSecurityResult {
        report: collector.finish("ci_security"),
        self_hosted_jobs: self_hosted.len(),
        pull_request,
    })
}

pub fn check_ci_workflow(path: &Path) -> Result<CiSecurityResult, CheckError> {
    let text = read_bounded_utf8(path)?;
    let mut result = find_ci_security_violations(&text)?;
    let display = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("workflow");
    for item in &mut result.report.findings {
        item.path = bounded_string(display);
    }
    Ok(result)
}

pub const CANONICAL_SKILLS: [&str; 6] = [
    "dev-coordinator",
    "formal-web-ui-verification",
    "full-repo-audit",
    "full-repo-test-coverage-audit",
    "ui-implementation-audit",
    "user-journey-docs-audit",
];

fn active_boundary_file(relative: &Path) -> bool {
    let name = slash_path(relative);
    if matches!(
        name.as_str(),
        "scripts/skills/check_repository_boundaries.py"
            | "scripts/skills/check_repository_boundaries_self_test.py"
            | "rust/tooling/src/repository_checks.rs"
            | "DecisionHistory.md"
            | "docs/holy-skills-snapshot.md"
    ) {
        return false;
    }
    let root_files = [
        ".gitmodules",
        "AGENTS.md",
        "CLAUDE.md",
        "README.md",
        "security-assumptions.md",
    ];
    let active_directories = [
        ".github",
        "deploy",
        "edge",
        "reference",
        "rust",
        "scripts",
        "skills",
        "src",
    ];
    root_files.contains(&name.as_str())
        || relative.components().next().is_some_and(|component| {
            active_directories
                .iter()
                .any(|item| component.as_os_str() == OsStr::new(item))
        })
}

fn boundary_rules(line: &str) -> Vec<&'static str> {
    let folded = line.to_ascii_lowercase();
    let mut rules = Vec::new();
    if let Some(index) = folded.find("/home/holyskills") {
        let following = folded
            .as_bytes()
            .get(index + "/home/holyskills".len())
            .copied();
        if following.is_none_or(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b'/' | b'\'' | b'"' | b'`')
        }) {
            rules.push("retired-checkout");
        }
    }
    let without_dot_git = folded.replace(".git", "");
    if let Some(index) = without_dot_git.find("/holyskills") {
        let prefix = &without_dot_git[..index];
        let owner = prefix.rsplit(['/', ':']).next().unwrap_or_default();
        let following = without_dot_git
            .as_bytes()
            .get(index + "/holyskills".len())
            .copied();
        if !owner.is_empty()
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            && following.is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_')
        {
            rules.push("retired-remote");
        }
    }
    if folded.contains("reference/codex-app-wide") {
        rules.push("retired-policy-path");
    }
    if ["skills/codex-dev-coordinator", "$codex-dev-coordinator"]
        .iter()
        .any(|needle| {
            folded.find(needle).is_some_and(|index| {
                folded
                    .as_bytes()
                    .get(index + needle.len())
                    .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            })
        })
    {
        rules.push("retired-skill-name");
    }
    if [
        "/opt/devcoordinator2/current",
        "/opt/devcoordinator2/releases",
    ]
    .iter()
    .any(|needle| {
        folded.find(needle).is_some_and(|index| {
            folded
                .as_bytes()
                .get(index + needle.len())
                .is_none_or(|byte| {
                    byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'\'' | b'"' | b'`')
                })
        })
    }) {
        rules.push("retired-release-source");
    }
    rules
}

/// Verify one live checkout, one universal policy, and exactly six skills.
pub fn audit_repository_boundaries(repository: &Path) -> Result<CheckReport, CheckError> {
    let root = canonical_directory(repository)?;
    let mut collector = FindingCollector::default();
    let skills = root.join("skills");
    let mut actual = BTreeSet::new();
    if skills.is_dir() {
        for entry in fs::read_dir(&skills).map_err(|_| {
            CheckError::new(CheckErrorKind::InputUnavailable, "cannot inspect skills")
        })? {
            let entry = entry.map_err(|_| {
                CheckError::new(
                    CheckErrorKind::InputChanged,
                    "skill entry changed during traversal",
                )
            })?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| {
                CheckError::new(
                    CheckErrorKind::InputChanged,
                    "skill entry changed during traversal",
                )
            })?;
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && entry.path().join("SKILL.md").is_file()
            {
                actual.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let expected = CANONICAL_SKILLS
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<BTreeSet<_>>();
    if actual != expected {
        collector.push(finding(
            "canonical-skill-set",
            "skills",
            None,
            "the repository must contain exactly the six canonical skill sources",
        ));
    }
    let policy = root.join("reference/universal/AGENTS.md");
    if fs::symlink_metadata(&policy)
        .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
        .unwrap_or(true)
    {
        collector.push(finding(
            "canonical-policy",
            "reference/universal/AGENTS.md",
            None,
            "regular policy file is missing",
        ));
    }
    for retired in ["reference/codex-app-wide", "skills/codex-dev-coordinator"] {
        if fs::symlink_metadata(root.join(retired)).is_ok() {
            collector.push(finding(
                "retired-path-present",
                retired,
                None,
                "retired path exists",
            ));
        }
    }
    if fs::symlink_metadata(root.join(".gitmodules")).is_ok() {
        collector.push(finding(
            "submodule-present",
            ".gitmodules",
            None,
            "submodules are not part of the unified source",
        ));
    }
    for relative in walk_tree(&root)? {
        if !active_boundary_file(&relative) {
            continue;
        }
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            CheckError::new(
                CheckErrorKind::InputChanged,
                "repository entry changed during traversal",
            )
        })?;
        if metadata.len() > MAX_TEXT_BYTES {
            continue;
        }
        let Ok(data) = fs::read(&path) else {
            continue;
        };
        if data.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(data) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            for rule in boundary_rules(line) {
                collector.push(finding(
                    rule,
                    &slash_path(&relative),
                    Some(index + 1),
                    "active source references retired ownership",
                ));
            }
        }
    }
    Ok(collector.finish("repository_boundaries"))
}

pub const REPOSITORY_STALE_EXIT: i32 = 2;
pub const REPOSITORY_REMOTE_UNAVAILABLE_EXIT: i32 = 3;
pub const REPOSITORY_USAGE_ERROR_EXIT: i32 = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFreshness {
    pub schema_version: u8,
    pub status: String,
    pub relation: String,
    pub ok: bool,
    pub repo: String,
    pub remote: String,
    pub branch: Option<String>,
    pub remote_ref: Option<String>,
    pub head: Option<String>,
    pub remote_head: Option<String>,
    pub merge_base: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub dirty: bool,
    pub fetched: bool,
    pub detail: Option<String>,
}

impl RepositoryFreshness {
    fn base(repo: &Path, remote: &str, dirty: bool, head: Option<String>) -> Self {
        Self {
            schema_version: 1,
            status: "remote-unavailable".to_owned(),
            relation: "unknown".to_owned(),
            ok: false,
            repo: bounded_string(&repo.to_string_lossy()),
            remote: bounded_string(remote),
            branch: None,
            remote_ref: None,
            head,
            remote_head: None,
            merge_base: None,
            ahead: 0,
            behind: 0,
            dirty,
            fetched: false,
            detail: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryFreshnessResult {
    pub exit_code: i32,
    pub payload: RepositoryFreshness,
}

impl RepositoryFreshnessResult {
    pub fn to_json(&self) -> Value {
        let payload = &self.payload;
        json!({
            "schema_version": payload.schema_version,
            "status": payload.status,
            "relation": payload.relation,
            "ok": payload.ok,
            "repo": payload.repo,
            "remote": payload.remote,
            "branch": payload.branch,
            "remote_ref": payload.remote_ref,
            "head": payload.head,
            "remote_head": payload.remote_head,
            "merge_base": payload.merge_base,
            "ahead": payload.ahead,
            "behind": payload.behind,
            "dirty": payload.dirty,
            "fetched": payload.fetched,
            "detail": payload.detail,
        })
    }
}

fn git_optional(repo: &Path, args: &[&str]) -> Result<Output, CheckError> {
    git_output(repo, args, false)
}

fn git_text(
    repo: &Path,
    args: &[&str],
    require_success: bool,
) -> Result<(bool, String), CheckError> {
    let output = git_output(repo, args, require_success)?;
    Ok((
        output.status.success(),
        utf8_stdout(&output)?.trim().to_owned(),
    ))
}

fn resolve_remote_branch(
    repo: &Path,
    remote: &str,
    requested: Option<&str>,
) -> Result<String, CheckError> {
    if let Some(requested) = requested {
        return Ok(requested
            .strip_prefix("refs/heads/")
            .unwrap_or(requested)
            .to_owned());
    }
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let (success, symbolic) = git_text(
        repo,
        &["symbolic-ref", "--quiet", "--short", &remote_head],
        false,
    )?;
    let prefix = format!("{remote}/");
    if success && symbolic.starts_with(&prefix) {
        return Ok(symbolic[prefix.len()..].to_owned());
    }
    let (success, advertised) = git_text(repo, &["ls-remote", "--symref", remote, "HEAD"], false)?;
    if success {
        for line in advertised.lines() {
            if let Some(branch) = line
                .strip_prefix("ref: refs/heads/")
                .and_then(|value| value.strip_suffix("\tHEAD"))
            {
                return Ok(branch.to_owned());
            }
        }
    }
    let (success, upstream) = git_text(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        false,
    )?;
    if success && upstream.starts_with(&prefix) {
        return Ok(upstream[prefix.len()..].to_owned());
    }
    for conventional in ["main", "master"] {
        let candidate = format!("refs/remotes/{remote}/{conventional}");
        if git_optional(repo, &["show-ref", "--verify", "--quiet", &candidate])?
            .status
            .success()
        {
            return Ok(conventional.to_owned());
        }
    }
    Err(CheckError::new(
        CheckErrorKind::GitFailed,
        format!("could not determine {remote}'s default branch"),
    ))
}

fn absolute_without_resolution(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Fetch and classify ancestry without changing working-tree files.
pub fn inspect_repository_freshness(
    repository: &Path,
    remote: &str,
    requested_branch: Option<&str>,
) -> Result<RepositoryFreshnessResult, CheckError> {
    let unresolved = absolute_without_resolution(repository);
    if !repository.is_dir() {
        let mut payload = RepositoryFreshness::base(&unresolved, remote, false, None);
        payload.status = "invalid-repository".to_owned();
        payload.detail = Some("repository directory does not exist".to_owned());
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_USAGE_ERROR_EXIT,
            payload,
        });
    }

    let (top_ok, top) = git_text(repository, &["rev-parse", "--show-toplevel"], false)?;
    if !top_ok {
        let mut payload = RepositoryFreshness::base(&unresolved, remote, false, None);
        payload.status = "invalid-repository".to_owned();
        payload.detail = Some("path is not inside a Git working tree".to_owned());
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_USAGE_ERROR_EXIT,
            payload,
        });
    }
    let canonical = PathBuf::from(top).canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::RepositoryUnavailable,
            "Git repository is unavailable",
        )
    })?;
    let (head_ok, head_text) = git_text(&canonical, &["rev-parse", "HEAD"], false)?;
    let head = head_ok.then_some(head_text);
    let (_, status) = git_text(
        &canonical,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
        true,
    )?;
    let dirty = !status.is_empty();
    let mut payload = RepositoryFreshness::base(&canonical, remote, dirty, head.clone());
    if head.is_none() {
        payload.status = "invalid-repository".to_owned();
        payload.detail = Some("repository has no HEAD commit".to_owned());
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_USAGE_ERROR_EXIT,
            payload,
        });
    }
    if !git_optional(&canonical, &["remote", "get-url", remote])?
        .status
        .success()
    {
        payload.detail = Some(format!("remote '{remote}' is not configured"));
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
            payload,
        });
    }
    let fetched = git_optional(
        &canonical,
        &["fetch", "--quiet", "--prune", "--no-tags", remote],
    )?;
    if !fetched.status.success() {
        payload.detail = Some(format!(
            "fetch from remote '{remote}' failed with exit code {}",
            fetched.status.code().unwrap_or(-1)
        ));
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
            payload,
        });
    }
    payload.fetched = true;
    let branch = match resolve_remote_branch(&canonical, remote, requested_branch) {
        Ok(branch) => branch,
        Err(error) => {
            payload.detail = Some(error.message);
            return Ok(RepositoryFreshnessResult {
                exit_code: REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
                payload,
            });
        }
    };
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    let (remote_ok, remote_head) =
        git_text(&canonical, &["rev-parse", "--verify", &remote_ref], false)?;
    if !remote_ok {
        payload.branch = Some(branch.clone());
        payload.remote_ref = Some(remote_ref);
        payload.detail = Some(format!(
            "remote branch '{branch}' was not found after fetch"
        ));
        return Ok(RepositoryFreshnessResult {
            exit_code: REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
            payload,
        });
    }
    let comparison = format!("HEAD...{remote_ref}");
    let (_, counts) = git_text(
        &canonical,
        &["rev-list", "--left-right", "--count", &comparison],
        true,
    )?;
    let values = counts.split_whitespace().collect::<Vec<_>>();
    if values.len() != 2 {
        return Err(CheckError::new(
            CheckErrorKind::GitOutputInvalid,
            "Git returned malformed ancestry counts",
        ));
    }
    let ahead = values[0].parse::<u64>().map_err(|_| {
        CheckError::new(
            CheckErrorKind::GitOutputInvalid,
            "Git returned malformed ancestry counts",
        )
    })?;
    let behind = values[1].parse::<u64>().map_err(|_| {
        CheckError::new(
            CheckErrorKind::GitOutputInvalid,
            "Git returned malformed ancestry counts",
        )
    })?;
    let (merge_ok, merge_base) = git_text(&canonical, &["merge-base", "HEAD", &remote_ref], false)?;
    let relation = match (ahead, behind) {
        (0, 0) => "current",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let status = if dirty && matches!(relation, "behind" | "diverged") {
        "dirty-on-stale-base"
    } else {
        relation
    };
    let ok = matches!(status, "current" | "ahead");
    payload.status = status.to_owned();
    payload.relation = relation.to_owned();
    payload.ok = ok;
    payload.branch = Some(branch);
    payload.remote_ref = Some(remote_ref);
    payload.remote_head = Some(remote_head);
    payload.merge_base = merge_ok.then_some(merge_base);
    payload.ahead = ahead;
    payload.behind = behind;
    payload.detail = (status == "dirty-on-stale-base").then(|| {
        "working tree changes were preserved; integrate the fetched remote in an isolated checkout"
            .to_owned()
    });
    Ok(RepositoryFreshnessResult {
        exit_code: if ok { 0 } else { REPOSITORY_STALE_EXIT },
        payload,
    })
}

pub const USER_ISSUE_LEDGER_HEADER: [&str; 5] = [
    "ID",
    "Applies to",
    "Mistake pattern",
    "Required behavior",
    "Prevention and verification",
];

const BROAD_LEDGER_SCOPES: [&str; 10] = [
    "all",
    "business",
    "businesslogic",
    "everything",
    "general",
    "issues",
    "misc",
    "miscellaneous",
    "shared",
    "userissues",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserIssueRow {
    pub issue_id: String,
    pub applies_to: String,
    pub mistake_pattern: String,
    pub required_behavior: String,
    pub prevention_and_verification: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerInspection {
    pub violations: Vec<String>,
    pub rows: Vec<UserIssueRow>,
    pub scope: Option<String>,
}

fn parse_ledger_row(line: &str, line_number: usize) -> Result<Vec<String>, String> {
    if !line.starts_with('|') || !line.ends_with('|') {
        return Err(format!(
            "line {line_number} must be a pipe-delimited table row"
        ));
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for character in line[1..line.len() - 1].chars() {
        if escaped {
            cell.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '|' {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(character);
        }
    }
    if escaped {
        cell.push('\\');
    }
    cells.push(cell.trim().to_owned());
    if cells.len() != USER_ISSUE_LEDGER_HEADER.len() {
        return Err(format!(
            "line {line_number} must contain exactly {} cells; escape literal pipes inside cells",
            USER_ISSUE_LEDGER_HEADER.len()
        ));
    }
    Ok(cells)
}

fn valid_issue_id(value: &str) -> bool {
    let Some(body) = value.strip_prefix("UIL-") else {
        return false;
    };
    let segments = body.split('-').collect::<Vec<_>>();
    if segments.len() < 2 {
        return false;
    }
    let (number, scope) = segments.split_last().expect("segments are not empty");
    number.len() >= 3
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && scope.iter().all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
}

fn normalize_ledger_cell(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches(['.', '!', '?'])
        .to_owned()
}

/// Validate one ledger document independently of its filesystem-owned scope.
pub fn inspect_user_issue_ledger(text: &str) -> LedgerInspection {
    let mut violations = Vec::new();
    let mut rows = Vec::new();
    let lines = text.lines().collect::<Vec<_>>();
    let scope = lines.first().and_then(|line| {
        line.strip_prefix("# User Issue Ledger: ")
            .and_then(|value| {
                (!value.is_empty() && value.trim() == value).then(|| value.to_owned())
            })
    });
    if scope.is_none() {
        violations.push("line 1 must match '# User Issue Ledger: <scope>'".to_owned());
        return LedgerInspection {
            violations,
            rows,
            scope,
        };
    }
    if lines.get(1) != Some(&"") {
        violations.push("the title must be followed by one blank line".to_owned());
    }
    if lines.len() < 4 {
        violations.push("ledger must contain the canonical five-column table".to_owned());
        return LedgerInspection {
            violations,
            rows,
            scope,
        };
    }
    let header = match parse_ledger_row(lines[2], 3) {
        Ok(header) => header,
        Err(error) => {
            violations.push(error);
            return LedgerInspection {
                violations,
                rows,
                scope,
            };
        }
    };
    if header.iter().map(String::as_str).collect::<Vec<_>>() != USER_ISSUE_LEDGER_HEADER {
        violations.push(format!(
            "table header must be exactly: {}",
            USER_ISSUE_LEDGER_HEADER.join(" | ")
        ));
    }
    let separator = match parse_ledger_row(lines[3], 4) {
        Ok(separator) => separator,
        Err(error) => {
            violations.push(error);
            return LedgerInspection {
                violations,
                rows,
                scope,
            };
        }
    };
    if separator.iter().any(|cell| cell != "---") {
        violations.push("table separator must contain exactly five '---' cells".to_owned());
    }
    if lines.len() == 4 {
        violations.push("an existing scoped ledger must contain at least one issue row".to_owned());
        return LedgerInspection {
            violations,
            rows,
            scope,
        };
    }

    let mut ids = HashSet::new();
    let mut patterns = HashSet::new();
    for (offset, line) in lines[4..].iter().enumerate() {
        let line_number = offset + 5;
        if line.is_empty() {
            violations.push(format!(
                "line {line_number} is blank; prose and spacing are not allowed after the table header"
            ));
            continue;
        }
        let cells = match parse_ledger_row(line, line_number) {
            Ok(cells) => cells,
            Err(error) => {
                violations.push(error);
                continue;
            }
        };
        let issue_id = &cells[0];
        if !valid_issue_id(issue_id) {
            violations.push(format!("line {line_number} ID must match UIL-<SCOPE>-NNN"));
        } else if !ids.insert(issue_id.clone()) {
            violations.push(format!("line {line_number} duplicates ID {issue_id}"));
        }
        for (column, value) in USER_ISSUE_LEDGER_HEADER[1..].iter().zip(&cells[1..]) {
            if value.is_empty() {
                violations.push(format!("line {line_number} has an empty '{column}' cell"));
            }
            if value.to_ascii_lowercase().contains("<br") {
                violations.push(format!(
                    "line {line_number} must keep '{column}' to one compact table cell"
                ));
            }
        }
        let pattern = (
            normalize_ledger_cell(&cells[1]),
            normalize_ledger_cell(&cells[2]),
        );
        if !pattern.0.is_empty() && !pattern.1.is_empty() && !patterns.insert(pattern) {
            violations.push(format!(
                "line {line_number} duplicates an existing applicability and mistake pattern; merge the rows"
            ));
        }
        rows.push(UserIssueRow {
            issue_id: cells[0].clone(),
            applies_to: cells[1].clone(),
            mistake_pattern: cells[2].clone(),
            required_behavior: cells[3].clone(),
            prevention_and_verification: cells[4].clone(),
        });
    }
    LedgerInspection {
        violations,
        rows,
        scope,
    }
}

fn valid_ledger_slug(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_uppercase())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn camel_tokens(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let start = index;
        if bytes[index].is_ascii_digit() {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        } else if bytes[index].is_ascii_uppercase() {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_uppercase() {
                index += 1;
            }
            if index - start > 1 && index < bytes.len() && bytes[index].is_ascii_lowercase() {
                index -= 1;
            } else {
                while index < bytes.len() && bytes[index].is_ascii_lowercase() {
                    index += 1;
                }
            }
        } else {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_lowercase() {
                index += 1;
            }
        }
        if index > start {
            tokens.push(value[start..index].to_ascii_lowercase());
        }
    }
    tokens
}

fn ledger_path_scope(relative: &Path) -> Result<(Vec<Vec<String>>, String), CheckError> {
    let mut components = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(stem) = relative.file_stem().and_then(OsStr::to_str) else {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger path does not define a scope",
        ));
    };
    components.push(stem.to_owned());
    let scopes = components
        .iter()
        .map(|component| camel_tokens(component))
        .collect::<Vec<_>>();
    if scopes.iter().any(Vec::is_empty) {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger path does not define a scope",
        ));
    }
    let namespace = format!(
        "UIL-{}-",
        scopes
            .iter()
            .flatten()
            .map(|token| token.to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join("-")
    );
    Ok((scopes, namespace))
}

fn ledger_title_scope(value: &str) -> Vec<Vec<String>> {
    value
        .split('/')
        .map(|component| {
            component
                .split(|character: char| !character.is_ascii_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.file_type() == right.file_type()
}

fn read_ledger_nofollow(path: &Path, root: &Path) -> Result<String, CheckError> {
    let root_before = fs::symlink_metadata(root).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger root changed while being read",
        )
    })?;
    if root_before.file_type().is_symlink() || !root_before.is_dir() {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "UserIssueLedgers must be a regular project-owned directory, not a symlink",
        ));
    }
    let canonical_root = root.canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger root changed while being read",
        )
    })?;
    let parent = path.parent().ok_or_else(|| {
        CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger has no parent directory",
        )
    })?;
    let parent_before = parent.canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger parent path changed while being read",
        )
    })?;
    if !parent_before.starts_with(&canonical_root) {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger parent traversal escaped the scoped root",
        ));
    }
    let path_before = fs::symlink_metadata(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger disappeared while being read",
        )
    })?;
    if path_before.file_type().is_symlink() || !path_before.is_file() {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "scoped ledger files must be regular files and must not be symlinks",
        ));
    }
    if path_before.len() > MAX_TEXT_BYTES {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger exceeds the bounded input size",
        ));
    }
    let file = File::open(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger disappeared while being read",
        )
    })?;
    let opened = file.metadata().map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger changed while being read",
        )
    })?;
    if !same_file(&path_before, &opened) {
        return Err(CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger path changed before it was opened",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            CheckError::new(
                CheckErrorKind::InputChanged,
                "ledger changed while being read",
            )
        })?;
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        return Err(CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger exceeds the bounded input size",
        ));
    }
    let path_after = fs::symlink_metadata(path).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger disappeared while being read",
        )
    })?;
    let root_after = fs::symlink_metadata(root).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger root path changed while being read",
        )
    })?;
    let parent_after = parent.canonicalize().map_err(|_| {
        CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger parent path changed while being read",
        )
    })?;
    if !same_file(&path_before, &path_after)
        || !same_file(&root_before, &root_after)
        || parent_before != parent_after
    {
        return Err(CheckError::new(
            CheckErrorKind::InputChanged,
            "ledger parent path changed while being read",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        CheckError::new(
            CheckErrorKind::InvalidInput,
            "ledger must contain valid UTF-8",
        )
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserIssueLedgerResult {
    pub report: CheckReport,
    pub ledger_count: usize,
}

impl UserIssueLedgerResult {
    pub fn is_clean(&self) -> bool {
        self.report.is_clean()
    }

    pub fn to_json(&self) -> Value {
        let mut report = self.report.to_json();
        if let Some(object) = report.as_object_mut() {
            object.insert("ledger_count".into(), self.ledger_count.into());
        }
        report
    }
}

struct LedgerScanState<'a> {
    root: &'a Path,
    collector: FindingCollector,
    ledger_count: usize,
    seen_ids: HashMap<String, String>,
    seen_patterns: HashMap<(String, String), String>,
    seen_scopes: HashMap<String, String>,
}

impl<'a> LedgerScanState<'a> {
    fn push(&mut self, rule: &str, path: &str, detail: impl Into<String>) {
        self.collector.push(CheckFinding {
            rule: rule.to_owned(),
            path: bounded_string(path),
            line: None,
            detail: detail.into(),
        });
    }

    fn inspect_file(&mut self, path: &Path, relative: &Path) {
        let display = slash_path(relative);
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            self.push(
                "ledger-name",
                &display,
                "ledger filenames must be UpperCamelCase .md slugs",
            );
            return;
        };
        if path.extension() != Some(OsStr::new("md")) || !valid_ledger_slug(stem) {
            self.push(
                "ledger-name",
                &display,
                "ledger filenames must be UpperCamelCase .md slugs",
            );
            return;
        }
        if BROAD_LEDGER_SCOPES.contains(&stem.to_ascii_lowercase().as_str()) {
            self.push(
                "ledger-scope",
                &display,
                "ledger scope is too broad; use a concrete surface or bounded business-logic perspective",
            );
        }
        let text = match read_ledger_nofollow(path, self.root) {
            Ok(text) => text,
            Err(error) => {
                self.push("ledger-read", &display, error.message);
                return;
            }
        };
        self.ledger_count += 1;
        let inspection = inspect_user_issue_ledger(&text);
        for message in &inspection.violations {
            self.push("ledger-format", &display, message.clone());
        }
        let owned_scope = ledger_path_scope(relative).ok();
        if let (Some(scope), Some((expected_scope, expected_prefix))) =
            (&inspection.scope, &owned_scope)
        {
            if ledger_title_scope(scope) != *expected_scope {
                self.push(
                    "ledger-title-scope",
                    &display,
                    "title scope must match its relative path; nested path components are separated with ' / '",
                );
            }
            let normalized_scope = scope
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            if let Some(previous) = self.seen_scopes.get(&normalized_scope).cloned() {
                self.push(
                    "duplicate-ledger-scope",
                    &display,
                    format!("duplicates scope title from {previous}"),
                );
            } else {
                self.seen_scopes.insert(normalized_scope, display.clone());
            }
            for row in &inspection.rows {
                if !row.issue_id.starts_with(expected_prefix) {
                    self.push(
                        "ledger-id-namespace",
                        &display,
                        format!(
                            "ID {} must use path-owned namespace {}<NNN>",
                            row.issue_id, expected_prefix
                        ),
                    );
                }
            }
        }
        for row in inspection.rows {
            if let Some(previous) = self.seen_ids.get(&row.issue_id).cloned() {
                self.push(
                    "duplicate-ledger-id",
                    &display,
                    format!("duplicates global ID {} from {previous}", row.issue_id),
                );
            } else {
                self.seen_ids.insert(row.issue_id.clone(), display.clone());
            }
            let pattern = (
                normalize_ledger_cell(&row.applies_to),
                normalize_ledger_cell(&row.mistake_pattern),
            );
            if let Some(previous) = self.seen_patterns.get(&pattern).cloned() {
                self.push(
                    "duplicate-ledger-pattern",
                    &display,
                    format!(
                        "duplicates applicability and mistake pattern from {previous}; keep one narrow owner"
                    ),
                );
            } else {
                self.seen_patterns.insert(pattern, display.clone());
            }
        }
    }
}

fn walk_ledgers(state: &mut LedgerScanState<'_>, current: &Path, relative: &Path) {
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(_) => {
            state.push(
                "ledger-traversal",
                &slash_path(relative),
                "cannot traverse scoped-ledger directory",
            );
            return;
        }
    };
    let mut entries = match entries.collect::<Result<Vec<_>, _>>() {
        Ok(entries) => entries,
        Err(_) => {
            state.push(
                "ledger-traversal",
                &slash_path(relative),
                "cannot traverse scoped-ledger directory",
            );
            return;
        }
    };
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let child_relative = relative.join(entry.file_name());
        let display = slash_path(&child_relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                state.push(
                    "ledger-traversal",
                    &display,
                    "cannot inspect scoped-ledger entry",
                );
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            let directory = fs::metadata(&path).is_ok_and(|target| target.is_dir());
            state.push(
                "ledger-symlink",
                &display,
                if directory {
                    "scoped-ledger directories must not be symlinks"
                } else {
                    "scoped ledger files must not be symlinks"
                },
            );
            continue;
        }
        if metadata.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !valid_ledger_slug(&name) {
                state.push(
                    "ledger-name",
                    &display,
                    "directory names must be concise UpperCamelCase slugs",
                );
                continue;
            }
            walk_ledgers(state, &path, &child_relative);
        } else if metadata.is_file() {
            state.inspect_file(&path, &child_relative);
        } else {
            state.push(
                "ledger-file-type",
                &display,
                "scoped ledger entries must be regular files",
            );
        }
    }
}

/// Validate the complete scoped user-issue ledger tree. An absent tree is valid.
pub fn audit_user_issue_ledgers(root: &Path) -> UserIssueLedgerResult {
    let mut state = LedgerScanState {
        root,
        collector: FindingCollector::default(),
        ledger_count: 0,
        seen_ids: HashMap::new(),
        seen_patterns: HashMap::new(),
        seen_scopes: HashMap::new(),
    };
    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        state.push(
            "ledger-parent-traversal",
            "UserIssueLedgers",
            "operator parent traversal is not allowed",
        );
        return UserIssueLedgerResult {
            report: state.collector.finish("user_issue_ledgers"),
            ledger_count: 0,
        };
    }
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return UserIssueLedgerResult {
                report: state.collector.finish("user_issue_ledgers"),
                ledger_count: 0,
            };
        }
        Err(_) => {
            state.push(
                "ledger-root",
                "UserIssueLedgers",
                "cannot inspect ledger root",
            );
            return UserIssueLedgerResult {
                report: state.collector.finish("user_issue_ledgers"),
                ledger_count: 0,
            };
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        state.push(
            "ledger-root",
            "UserIssueLedgers",
            "UserIssueLedgers must be a regular project-owned directory, not a symlink",
        );
    } else {
        walk_ledgers(&mut state, root, Path::new(""));
        if state.ledger_count == 0 {
            state.push(
                "ledger-empty",
                "UserIssueLedgers",
                "an existing UserIssueLedgers directory must contain at least one scoped ledger",
            );
        }
    }
    UserIssueLedgerResult {
        report: state.collector.finish("user_issue_ledgers"),
        ledger_count: state.ledger_count,
    }
}

pub const REQUIRED_POLICY_SECTIONS: [&str; 13] = [
    "1. Infer the intended outcome and carry it to completion",
    "2. Load relevant context and keep evidence bounded",
    "3. Apply approval and security gates proportionally",
    "4. Keep decisions, unfinished outcomes, and executions separate",
    "5. Coordinate tools, delegated work, and asynchronous execution",
    "6. Deliver preliminary results continuously",
    "7. Validate at stable checkpoints without disrupting progress",
    "8. Keep product behavior and completion claims truthful",
    "9. Verify UI journeys and put requested content first",
    "10. Preserve lessons from confirmed agent mistakes",
    "11. Protect canonical sources, data, and running systems",
    "12. Explain results through the user's goals and experience",
    "13. Review outcomes and resource use every 24 hours",
];

const FORBIDDEN_POLICY_NAMES: [&str; 13] = [
    "Codex",
    "Claude",
    "ImageGen",
    "Next.js",
    "Vercel",
    "Swift",
    "macOS",
    "Docker",
    "PostgreSQL",
    "systemd",
    "formal-web-ui-verification",
    "dev-coordinator",
    "postgres-docker-backup",
];

fn policy_section<'a>(text: &'a str, heading: &str) -> &'a str {
    let marker = format!("## {heading}");
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        if segment.trim_end() == marker {
            let start = offset + segment.len();
            let tail = &text[start..];
            let end = tail
                .find("\n## ")
                .map(|index| index + 1)
                .unwrap_or(tail.len());
            return &tail[..end];
        }
        offset += segment.len();
    }
    ""
}

fn fold_policy(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn require_policy_terms(violations: &mut Vec<String>, body: &str, label: &str, terms: &[&str]) {
    let folded = fold_policy(body);
    let missing = terms
        .iter()
        .filter(|term| !folded.contains(&term.to_lowercase()))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        violations.push(format!(
            "{label} missing required concepts: {}",
            missing.join(", ")
        ));
    }
}

const POLICY_CONTRACTS: &[(&str, usize, &[&str])] = &[
    (
        "project terminology ownership",
        8,
        &[
            "effective project glossary",
            "shared terminology",
            "concept and language",
            "localization architecture and exact messages",
            "reading the glossary is not proof",
        ],
    ),
    (
        "intent-driven completion contract",
        0,
        &[
            "infer the user's intent and task scope",
            "prior conversation",
            "bias toward action",
            "action-oriented expressions",
            "instructions to perform the work",
            "do not stop at acknowledgement",
            "implementation, integration, and verification",
            "do not settle for a partial",
            "save time, effort, or tokens",
            "interpret broad requests broadly",
            "conventionally expected capabilities",
            "operating context, relevant standards",
            "authoritative sources",
            "security-assumptions gate",
            "reasonably implied",
            "unrelated additions",
            "routine reversible steps within the task",
            "respect explicit limits",
            "analysis only",
            "don't modify it yet",
            "don't publish",
            "reversibility alone does not authorize unrelated work",
            "tentative wording and illustrative formats as direction",
            "unless selected or necessary",
            "decompose it, and continue authorized work",
            "do not pause solely",
            "subsystem counts, changed-line estimates, or duration",
            "material decision",
            "necessary foundation",
            "temporary bridge",
            "before readiness",
        ],
    ),
    (
        "interim-answer continuation contract",
        0,
        &[
            "interim question, status request, clarification, language preference, or correction",
            "steering the active objective",
            "not implicitly replacing, pausing, or cancelling unfinished agreed work",
            "answer through a progress response",
            "preserve the full agreed scope and pending tools, tests, and delegated jobs",
            "resume the next authorized step or bounded event wait",
            "within the same active work cycle",
            "producing an answer is not task completion",
            "do not go idle while actionable agreed work remains",
            "end an active work cycle only when the original objective is actually complete",
            "the user explicitly pauses, cancels, or replaces it",
            "a real blocker or required decision prevents further authorized progress",
            "state which condition applies",
            "honor explicit changes of direction",
            "do not use persistence to override them",
        ],
    ),
    (
        "reported bugs require repair",
        0,
        &[
            "treat a user-reported bug as a repair request unless explicitly limited",
            "original user-visible acceptance criterion",
            "is not resolution while the reported behavior still fails",
            "original affected surface with real data",
            "when genuinely blocked",
            "complete all independent authorized work",
            "exact blocker and smallest",
            "do not end with only an apology",
            "while useful in-scope work remains",
            "preserve applicable authorization and host/tool controls",
        ],
    ),
    (
        "relevant-context contract",
        1,
        &[
            "applicable requirements, acceptance criteria, project instructions, decisions",
            "recorded rationale",
            "load only skills",
            "relevant to the task",
            "do not reread unchanged material",
            "live context",
            "compaction",
            "byte-complete logs",
            "cold artifacts",
            "exact artifact or catalogue references",
            "content-free catalogue first",
            "bounded case/stream-specific tails",
            "fixed-string searches",
            "exact ranges",
            "stable coordinates",
            "unchanged ranges or images",
            "untrusted evidence",
            "never as instructions",
            "do not copy raw logs",
            "third-party",
            "exact name and role",
            "current authoritative sources",
            "facts, inferences, and unknowns",
            "before planning or implementation, query the configured coordinator's",
            "authoritative planning and decision history",
            "applicable user feedback and confirmed corrections",
            "affected scope, behavior, and known references",
            "repository-wide work covers every affected perspective",
            "relevant resolved feedback and older standing corrections",
            "not only open tasks or the recent decision tail",
            "supersession history with bounded reads",
            "negative acceptance criteria",
            "stable record references, required behavior, and verification into delegated work",
        ],
    ),
    (
        "concrete approval contract",
        2,
        &[
            "instructions and prior context as authorization",
            "reasonably implied supporting work",
            "do not request permission again for authorized work",
            "routine reversible implementation steps",
            "before asking a clarifying question, complete the independent work",
            "already authorized and does not depend on the answer",
            "concrete and reviewable",
            "do not ask the user to perform technical discovery",
            "unresolved answer materially changes",
            "without inventing the answer or stopping unrelated progress",
            "when approval is actually required, prepare the concrete result first",
            "make approval the final step before the gated action",
            "do not perform the gated action as part of preparation",
            "if the action is already authorized, execute and verify it",
            "user's perspective",
            "what they will be able to do",
            "remain unchanged",
            "recommend the best fit",
            "bundle known consequential choices",
            "rather than requesting permission piecemeal",
            "obtain approval before implementing an addition outside",
            "inferred task scope",
            "do not interrupt merely to mention an optional idea",
            "do not introduce unsolicited warnings",
            "because of hypothetical risks",
            "concrete evidence, confirmed requirements, or an applicable control",
            "approval applies to the described outcome and boundaries",
            "plain “yes” is sufficient",
            "require the user to transcribe",
            "prescribed technical phrases",
            "invoke mandatory host or tool approval controls directly",
            "do not bypass them",
            "ask again only when new evidence materially changes",
        ],
    ),
    (
        "security-assumptions gate",
        2,
        &[
            "before proposing or making a decision that adds, changes, weakens, removes, or intentionally omits",
            "security control, read project-root `security-assumptions.md`",
            "every such decision and resulting measure must cite",
            "project-specific, user-confirmed assumptions",
            "not confirmed project facts",
            "users and operators",
            "runtime environment and ownership",
            "assets and data sensitivity",
            "credible adversaries and misuse",
            "trust boundaries",
            "necessary and explicitly unnecessary gates",
            "acceptable risks",
            "review triggers",
            "record is absent or insufficient",
            "confirmed requirements and read-only discovery first",
            "unresolved material assumption",
            "unnecessary control",
            "omit a necessary one",
            "meaningful rework",
            "smallest concise set of unresolved material questions",
            "record the confirmed answers",
            "do not repeat resolved questions",
            "demand a full baseline unless",
            "unknowns explicitly; they cannot justify a control",
            "non-security work does not trigger a security interview",
            "routine use of a reviewed skill or tool",
            "preserves its documented controls",
            "does not reopen the assumptions record",
            "relevance, not permission to expand scope",
            "approval and assumption gates are cumulative",
            "never default to blanket hardening",
            "disposable test data",
            "known single-user environment",
            "without a requirement or contrary evidence",
        ],
    ),
    (
        "decision-memory contract",
        3,
        &[
            "configured software-owned database",
            "completion ledger and decision history",
            "never substitute files or chat memory",
            "database unavailability blocks the affected completion claim",
            "decision_record",
            "materially distinct options",
            "technical_note",
            "supersedes",
            "stable `ref`",
            "decision_tail",
            "decision_search",
            "before retrying an option",
            "summary_due",
            "decision_summarize",
            "before continuing",
            "durable direction",
            "distinguish confirmed decisions from inferred patterns",
            "cite decision refs",
            "append-only maintenance needs no additional user approval",
        ],
    ),
    (
        "completion-versus-execution contract",
        3,
        &[
            "durable unfinished outcomes, not execution attempts",
            "passed, failed, cancelled, timed-out, invalidated, retried, or superseded run",
            "governed run history",
            "execution failures and suggestions require diagnosis before becoming ledger outcomes",
            "create or reopen a task only when evidence establishes",
            "reuse an existing task only when its concrete outcome and acceptance scope match",
            "passing runs do not automatically complete work",
            "execution-only actions are not tasks",
            "missing test or harness capability",
            "running or rerunning it is not",
            "keep every agreed gap open until verified or explicitly removed from scope by the user",
            "knowingly leaving in-scope functionality temporarily unimplemented establishes an unfinished outcome",
            "append-only history",
            "externally blocked outcomes open",
            "write tasks for a non-specialist",
            "remaining outcome, user impact, unblock condition",
            "observable completion proof",
            "structured evidence references",
            "keep compact run receipts",
            "do not copy run status, logs, or failure narratives into task history",
            "no request-related unfinished outcome and fresh required verification evidence",
        ],
    ),
    (
        "tool-orchestration contract",
        4,
        &[
            "partition tool calls into dependency layers",
            "safe independent calls in the same layer concurrently",
            "serialize only real dependencies",
            "conflicting mutations",
            "bounded programmatic orchestration",
            "pagination, filtering, joins, deduplication, and aggregation",
            "compact structured conclusions, evidence, and errors",
        ],
    ),
    (
        "asynchronous-execution contract",
        4,
        &[
            "prefer asynchronous or nonblocking execution when supported and useful",
            "builds, tests, packaging, publication",
            "continue other useful work instead of waiting",
            "await a result when it is needed for the next dependent action",
            "not merely because a background operation is running",
            "not fire-and-forget",
            "retain operation identities",
            "observe completion and failures",
            "preserve evidence",
            "required cleanup",
            "submission alone is never proof of success",
            "account for every required background operation and verify its result",
            "do not leave necessary work running unobserved",
        ],
    ),
    (
        "contract-ready delegation contract",
        4,
        &[
            "delegate implementation only after shared schemas, directory layouts, ownership boundaries",
            "one cross-component acceptance fixture are fixed",
            "no unresolved shared-interface decision",
            "overlapping mutable-file ownership",
            "rather than a fixed implementation- agent count",
            "parent remains the sole integration owner",
            "nested implementation delegation requires explicit parent authorization",
            "that independent branch",
        ],
    ),
    (
        "parallel-work contract",
        4,
        &[
            "governed dependency-ready work",
            "configured host-wide scheduler",
            "do not invent local worker limits, fake dependencies",
            "second capacity controller",
            "cheap checks that can invalidate expensive downstream evidence real success dependencies",
            "ordinary failure does not cancel safe siblings",
            "record the missing capability",
            "best supported execution without false claims",
        ],
    ),
    (
        "event-wait contract",
        4,
        &[
            "blocking event subscriptions rather than model-turn status polling",
            "expected-event deadline",
            "multiplex pending subscriptions",
            "all due heartbeats through one shared scheduler",
            "fetch bounded authoritative state once",
            "advance its cursor",
            "one service-owned watcher may poll using bounded backoff",
            "resetting on a meaningful state change",
            "the agent does not poll",
            "watcher a cancellation path and expected-event deadline",
            "timeouts are failure ceilings",
            "not a blanket 100 ms interval",
            "deduplicate watchers for the same obligation",
        ],
    ),
    (
        "continuous preliminary delivery contract",
        5,
        &[
            "throughout implementation",
            "earliest meaningful, runnable increment",
            "authorized non-production surface",
            "application build, executable",
            "do not wait for feature completion or broad validation",
            "refresh the available result promptly",
            "coherent, runnable increments",
            "continuing development activity, not a one-time preview",
            "moving concurrently wherever independent",
            "user inspection must not gate unrelated work",
            "narrowest relevant checks",
            "safely runnable",
            "affected rendered interactions",
            "inspect and guide the work throughout development",
            "incorporate feedback promptly",
            "do not wait for acknowledgement unless a material decision genuinely requires it",
            "exact access or launch instructions",
            "label results preliminary",
            "not final readiness or final visual approval",
            "reuse established surfaces",
            "respect declared shared environments",
            "rather than creating unnecessary per-agent environments",
            "preliminary delivery does not reduce the final agreed result",
        ],
    ),
    (
        "purpose-aware delivery timing contract",
        5,
        &[
            "without a meaningful deployable result are performance-only",
            "no delivery targets, obligations, or alarms",
            "project age alone never creates a delivery obligation",
            "authorized implementation transition with a real target starts delivery timing",
            "agent runtime owns durable clocks, deadline revisions, deduplicated wakeups",
            "coordinator owns authoritative outcomes, decisions, evidence, and capacity",
            "delivery interval defaults to 24 hours",
            "defaults to 36 hours",
            "explicit user postponement, pause, or threshold changes revise the applicable obligation",
            "retaining the prior revision, reason, confirmation, and actual timestamps",
            "invalidate obsolete timer wakes and immediately reevaluate existing blocks",
            "excluding earlier performance-only work",
            "qualified deliveries and completed reviews are separate receipts",
            "exactly one delivery request for that scope and deadline revision while independent coding continues",
            "repeated wakes do not duplicate the request",
            "before the hard-stop threshold never blocks ordinary implementation",
            "block ordinary implementation throughout the affected delivery scope",
            "allow only delivery recovery, necessary diagnosis and repairs",
            "independent projects may continue",
        ],
    ),
    (
        "standing preview and browser-QA permission contract",
        5,
        &[
            "standing permission covers in-scope local browser automation",
            "configured development coordinator's local runtime work without separate chat authorization",
            "preserve the tools' documented controls",
            "does not expand scope or authorize production changes",
            "destructive data actions",
            "credential or trust changes",
            "new infrastructure outside the agreed work",
            "bypassing host/tool approval controls",
        ],
    ),
    (
        "semantic-checkpoint validation contract",
        6,
        &[
            "cheap checks and focused tests",
            "invalidate the current design or changed behavior",
            "complete coherent implementation batches before broader validation",
            "do not run the complete suite after each plan item, edit, commit, or delegated result",
            "pre-merge validation once shared interfaces and integrations are stable",
            "complete release validation against a frozen candidate",
            "source, configuration, running surface, and evidence unchanged",
            "mutable development surface continues evolving",
            "a run proves only its exact snapshot",
            "do not justify stopping or restarting it",
            "does not establish readiness for a later candidate",
            "one final complete pass over the final frozen candidate",
            "one agent owns complete-suite execution",
            "delegated agents run focused checks unless assigned the sealed integration pass",
            "test-plumbing-only changes do not trigger another complete release pass",
            "implementation and test infrastructure are frozen",
        ],
    ),
    (
        "complete-cycle contract",
        6,
        &[
            "first ordinary failure",
            "finite sealed test, audit, rehearsal, or deployment run",
            "begin diagnosis and repair immediately in separate isolated state",
            "scope, approval, and mistake-prevention gates",
            "let the original run finish collecting every safe finding",
            "do not inject fixes into its running surface, restart it, or destroy its evidence",
            "rather than creating tasks as they appear",
            "stop or mitigate immediately only when",
            "security or safety harm",
            "data loss",
            "shared-state corruption",
            "destruction of useful evidence",
            "results invalid enough",
            "reconcile all findings",
            "promote only diagnosed durable gaps",
            "batch related fixes",
            "focused checks during repair",
            "one final complete pass",
        ],
    ),
    (
        "verification contract",
        6,
        &[
            "acceptance criteria",
            "success, edge, failure, integration, and recovery paths",
            "same visible or operational surface",
            "do not substitute an internal unit",
            "promised end-to-end behavior",
            "recall and precision",
            "realistic must-catch failures",
            "each advertised class",
            "false-positive guards for intentional patterns",
        ],
    ),
    (
        "implemented-product-behavior contract",
        7,
        &[
            "never present invented facts, data, measurements",
            "statuses, results, actions, integrations, or controls",
            "real sources, user input, measurement, imported data",
            "explicitly requested deterministic definitions",
            "data-dependent feature is complete only when",
            "real data, persistence, processing, failure states",
            "visible result work end to end",
            "loading, error, empty, or unavailable state",
            "every visible enabled control must perform its stated action",
            "rendered interface",
            "expected downstream result",
            "is not proof of promised navigation, persistence, processing, or integration",
            "do not expose placeholders",
            "empty handlers, no-op links, fake success",
            "enabled product ui",
            "synthetic examples remain isolated",
            "declared mock-data prototypes",
            "interactions work within their stated boundary",
            "specification requires communicating future availability",
            "semantically disabled",
            "non-actionable, visibly unavailable, and specifically tracked",
            "out-of-scope future information is noninteractive content",
            "preliminary results may honestly contain unfinished scope",
            "final completion may not",
            "missing, simulated, inert",
            "open request-related ledger entry",
        ],
    ),
    (
        "UI interaction completion contract",
        8,
        &[
            "before reporting ui complete",
            "one evidence pass over only the agreed screens",
            "journeys, states, and responsive variants",
            "does not authorize a broader exhaustive audit",
            "inventory every visible interactive element, including conditional ones",
            "journey, action, and expected observable result",
            "verify the downstream result",
            "success, cancellation, validation failure, permission failure",
            "recovery where applicable",
            "reload when persistence is promised",
            "finish the safe diagnostic pass",
            "isolated repair",
            "batch-fix",
            "zero enabled controls without real behavior",
            "zero requested journeys without rendered end-to-end evidence",
            "zero request-related unfinished outcomes",
            "geometry checks support evidence but do not replace interaction verification",
        ],
    ),
    (
        "interface-content contract",
        8,
        &[
            "content promise",
            "first substantial",
            "initial viewport, including narrow screens",
            "supporting rather than displacing",
            "collection destinations do not lead with add or edit forms",
            "destinations explicitly dedicated to creating or editing one item",
            "creation immediately reveals a focused dialog",
            "current viewport—not below a long list",
            "success reveals the new item",
            "cancellation restores context and focus",
            "controls beside the affected object",
            "destructive actions name an explicit target and state",
            "simple normal first input before inferred or advanced fields",
            "prefer one concise heading or label",
            "supporting copy only when requested or necessary",
            "prevent misunderstanding or error",
            "do not expose private values, internal identifiers, serialized payloads",
            "purpose-built controls",
            "wide and narrow layouts",
            "loading, empty, error, populated, and long-content states",
            "functional defect",
            "new interface or substantial redesign",
            "routine fixes do not require three new proposals",
            "options, selection or approval state",
            "exact outstanding response request",
            "when no follow-up can appear",
        ],
    ),
    (
        "agent-mistake contract",
        9,
        &[
            "distinguish agent mistakes from changed user intent",
            "external state",
            "finish its useful diagnostic cycle",
            "nearest durable prevention layer",
            "before the product fix",
            "locate the relevant feedback, unfinished outcome, and standing correction",
            "coordinator's planning and decision tools",
            "reuse existing outcome tasks",
            "append or supersede the durable correction",
            "preserve its record references",
            "immediate mitigation prevents harm or data loss",
            "preserve evidence and mitigate first",
            "batch the guardrail and implementation changes",
            "plausibly adjacent paths",
            "retest the original surface",
            "generalized repeatable lessons in policy",
            "narrow guarantees",
            "one-off narratives out of policy",
            "unresolved user feedback in the authoritative task ledger",
            "confirmed, repeatable correction through `decision_record`",
            "linked to the relevant feedback or outcome task",
            "completed task does not retire its standing correction",
            "feedback alone is not proof of an agent mistake",
            "explicit applicability",
            "confirmed mistake pattern, required behavior, prevention and verification",
            "references to its supporting feedback or evidence",
            "implementation detail in `technical_note`",
            "supported decision aspects",
            "ui, automation, coding-style, math, data, security, operations, testing, documentation",
            "each affected business perspective",
            "do not invent tool fields or maintain parallel file ledgers",
            "service record ids and stable refs",
            "preserve legacy correction ids as provenance",
            "search them before creating a duplicate",
            "file paths no longer own correction identities",
            "append-only record or `supersedes`",
            "do not classify changed intent, new scope, external failures",
            "unconfirmed agent-found concerns as confirmed mistakes",
            "raw conversation and incident narration out of durable corrections",
            "corrections remain discoverable after fixes",
            "explicit recorded decision and preserved history",
            "not task closure or deletion of the earlier record",
            "legacy file-based ledgers are historical reference material",
            "not another writable authority or an offline fallback",
            "do not create or update them",
            "delete historical files merely because authority moved",
            "claim their contents were migrated without checking the authoritative records",
            "durable prevention in decision history",
            "unfinished outcomes in the task ledger",
            "execution or incident evidence in its governed history",
            "without treating one as a substitute for another",
        ],
    ),
    (
        "source-and-system protection contract",
        10,
        &[
            "canonical sources as the only writable truth",
            "verified source workflow",
            "current remote",
            "remote-unavailable means unknown",
            "never discard, hide, stash, reset, or rewrite valuable dirty work",
            "evidence-backed merge",
            "verified baseline",
            "running service, shared resource, or persistent store",
            "coordination, locking, backup, and recovery",
            "preserve failure evidence before restarting",
            "verify recovery through the same surface",
            "recoverable backup",
            "disposable and isolated",
            "tests that create persistent state isolate or safely clean up their own state",
            "dependencies and concurrent runs",
            "never unconditionally delete shared records",
            "explicit working directories",
            "unambiguous mutation targets",
            "verify the intended result",
            "domain meaning, ownership, lifecycle, reuse, validation, and evidence needs",
            "shared transport or presentation does not imply shared ownership",
        ],
    ),
    (
        "user-goal explanation contract",
        11,
        &[
            "perspective of the user and their requirements",
            "not from the implementation's internal structure",
            "what the user wants to accomplish",
            "what people can now do",
            "what remains incomplete",
            "observable effect",
            "do not substitute jargon, acronyms, component names",
            "naming a mechanism does not explain its purpose or consequence",
            "explain it in the user's context before using its technical name",
            "what the user is choosing",
            "how the choices differ in actual use",
            "best satisfies their requirements",
            "explain verification through the behavior it demonstrates",
            "not only through commands, test names, or pass counts",
            "optional technical detail after the user-facing account",
            "report meaningful preliminary results promptly",
            "without making user acknowledgement a condition",
            "facts, inferences, assumptions, and genuine blockers",
            "do not claim fixed, ready, complete, or done while",
            "request-related completion work remains open",
        ],
    ),
    (
        "daily and deduplicated performance review contract",
        12,
        &[
            "including performance-only specifications and research",
            "24 elapsed hours and every subsequent 24 hours",
            "a review does not reset a delivery deadline",
            "measured tokens, active agent time, elapsed time, and waiting time",
            "do not double-count parent and descendant totals",
            "user waiting, inactive periods, and intentional required release revalidation are not waste by themselves",
            "prioritize elapsed time to the next useful result",
            "without shrinking the agreed result or weakening required verification",
            "deduplicated by scope, cause, and relevant evidence revision",
            "automatically implement only within the reviewed repository and current authorized scope",
            "specification-only work authorizes improvements to that work, not product implementation",
            "name the observed delay, affected outcome, scope, baseline, evidence and uncertainty",
            "compare materially different actions, including keeping the current approach",
            "success measure and reversible keep/revert condition before changing work",
            "compare the same observable outcome before and after",
            "keep demonstrated improvement, revert an ineffective or harmful change safely",
            "mark evidence inconclusive",
            "never report only resource totals",
        ],
    ),
];

fn contains_ordered(text: &str, terms: &[&str]) -> bool {
    let folded = fold_policy(text);
    let mut offset = 0usize;
    for term in terms {
        let term = term.to_lowercase();
        let Some(found) = folded[offset..].find(&term) else {
            return false;
        };
        offset += found + term.len();
    }
    true
}

fn policy_clauses(text: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut start = 0usize;
    for (index, character) in text.char_indices() {
        if matches!(character, '.' | '!' | '?' | ';' | '\n') {
            if index > start {
                let folded = fold_policy(&text[start..index]);
                if !folded.is_empty() {
                    clauses.push(folded);
                }
            }
            start = index + character.len_utf8();
        }
    }
    if start < text.len() {
        let folded = fold_policy(&text[start..]);
        if !folded.is_empty() {
            clauses.push(folded);
        }
    }
    clauses
}

fn positive_literal_match(clause: &str, needle: &str) -> bool {
    let Some(index) = clause.find(needle) else {
        return false;
    };
    let prefix = clause[..index].trim_end();
    ![
        "never",
        "do not",
        "don't",
        "must not",
        "may not",
        "not",
        "cannot",
        "can't",
        "fail to",
        "fails to",
        "insufficient to",
    ]
    .iter()
    .any(|negative| prefix.ends_with(negative))
}

const INTENT_POLICY_CONTRADICTIONS: &[(&str, &str)] = &[
    (
        "end the work cycle after answering an interim question while authorized work remains",
        "interim answers must resume unfinished authorized work",
    ),
    (
        "answer a status question and abandon pending tests or delegated jobs",
        "status answers must preserve pending operations",
    ),
    (
        "treat a language correction as cancellation of unfinished work",
        "language corrections must not implicitly cancel work",
    ),
    (
        "replace the active objective with every mid-task clarification",
        "clarifications must not implicitly replace the active objective",
    ),
    (
        "ignore an explicit pause, cancellation, or replacement and continue the original work",
        "explicit changed direction must be honored",
    ),
    (
        "continue the gated action despite a real required user decision",
        "continuation must respect real required decisions",
    ),
    (
        "keep working after the objective is complete by inventing more tasks",
        "actual completion permits stopping without invented work",
    ),
    (
        "restart a finite diagnostic pass after answering a status question",
        "interim answers must preserve finite diagnostic passes",
    ),
    (
        "explain capability and offer to continue instead of doing the work",
        "action-oriented requests require action rather than capability-only replies",
    ),
    (
        "implement only the features individually enumerated in a broad product request",
        "broad requests must include reasonably implied end-to-end capabilities",
    ),
    (
        "reversible actions authorize unrelated work",
        "reversibility does not authorize unrelated work",
    ),
    (
        "always ask for permission before routine reversible steps already authorized by the task",
        "authorized routine work must not require another conversational approval",
    ),
    (
        "perform the gated external write before asking for approval",
        "reviewable preparation must not perform the gated action",
    ),
    (
        "add safety checklists for every hypothetical risk",
        "hypothetical risks must not create unsolicited safety checklists",
    ),
    (
        "pause for approval solely because the task exceeds 1,000 changed lines",
        "size alone must not stop authorized work",
    ),
    (
        "wait for every asynchronous build before continuing independent work",
        "asynchronous operations must not block independent authorized work",
    ),
    (
        "treat submitting a background operation as proof of completion",
        "asynchronous submission must not be presented as completion",
    ),
    (
        "only ui changes require preliminary results",
        "continuous preliminary delivery must include non-UI outcomes",
    ),
    (
        "publish one preview and wait until final completion to update it",
        "preliminary results must continue updating during implementation",
    ),
    (
        "wait for user acknowledgement before continuing unrelated development",
        "user inspection must not gate unrelated development",
    ),
    (
        "reuse passing evidence from an older snapshot to claim the latest candidate ready",
        "validation evidence must remain bound to its actual snapshot",
    ),
    (
        "technical component names alone are sufficient to explain user outcomes",
        "explanations must describe user goals and consequences rather than only mechanisms",
    ),
];

fn literal_policy_contradictions(text: &str) -> Vec<String> {
    const RULES: &[(&str, &str)] = &[
        (
            "always reread every unchanged file",
            "context must remain relevant and non-redundant",
        ),
        (
            "load every skill or tool",
            "context must remain relevant and non-redundant",
        ),
        (
            "implement unrequested hypothetical hardening without asking",
            "unrequested engineering must never proceed without informed approval",
        ),
        (
            "proceed with privacy hardening and backup infrastructure without approval",
            "risk-control expansion must not proceed without user approval",
        ),
        (
            "ask for approval before completing the available read-only investigation",
            "approval requests must follow available read-only investigation",
        ),
        (
            "request approval piecemeal as each implementation detail emerges",
            "known consequential approval effects must be bundled into one decision",
        ),
        (
            "approval request may contain technical jargon without plain language",
            "approval requests must explain the problem and outcome in plain language",
        ),
        (
            "approval applies only to the named implementation details",
            "approval must cover the recorded outcome and boundaries, not only implementation details",
        ),
        (
            "a plain “yes” is insufficient",
            "a plain yes must be sufficient for the described outcome and boundaries",
        ),
        (
            "user must repeat the internal identifier",
            "users must never repeat internal identifiers or prescribed technical phrases",
        ),
        (
            "user must reply with the exact phrase",
            "users must never repeat internal identifiers or prescribed technical phrases",
        ),
        (
            "automatically preserve and migrate all user data even when not requested",
            "hypothetical engineering must not expand scope automatically",
        ),
        (
            "under-engineering is acceptable",
            "explicitly agreed work must remain mandatory",
        ),
        (
            "implementation gaps are more punishable than reasonable over-engineering",
            "scope policy must not rank under-engineering against over-engineering",
        ),
        (
            "reasonable over-engineering is more serious than under-engineering",
            "scope policy must not rank under-engineering against over-engineering",
        ),
        (
            "silent scope expansion is permitted",
            "silent scope expansion must remain prohibited",
        ),
        (
            "are mandatory delivery requirements",
            "tentative or illustrative language must not become mandatory by default",
        ),
        (
            "work is always in scope regardless of acceptance",
            "supporting engineering must not enter scope without a confirmed requirement or necessity",
        ),
        (
            "continue without a pause or scope explanation",
            "hidden large scope must not continue without one scope explanation",
        ),
        (
            "more checks, parsers, adapters, and supported formats always make",
            "extra machinery must not be treated as proof of completeness",
        ),
        (
            "every optional idea the agent merely notices must trigger",
            "optional ideas that will not be implemented must not trigger blanket questions",
        ),
        (
            "implement an unrequested edge-case recovery flow without user approval",
            "unrequested engineering must never proceed without informed approval",
        ),
        (
            "imagined edge case counts as a credible project need",
            "a hypothetical edge case must not establish project scope",
        ),
        (
            "small agreed behavior does not need to be recorded",
            "no agreed gap is too small for the completion ledger",
        ),
        (
            "may use implementation jargon without plain language",
            "completion-ledger technical detail must not replace a plain-language outcome and impact",
        ),
        (
            "maintain project-root `completionledger",
            "CompletionLedger.md must never be a writable ledger",
        ),
        (
            "is the default authoritative completion store",
            "CompletionLedger.md must never be authoritative or default",
        ),
        (
            "may fall back to `completionledger",
            "database ledger failure must never fall back to Markdown",
        ),
        (
            "delete verified completion-ledger issues after each release",
            "implemented completion-ledger history must remain permanent",
        ),
        (
            "production ui may show plausible synthetic numbers and results",
            "production UI must not use invented stand-in values",
        ),
        (
            "stop the test at the first failure",
            "diagnostic cycles must not stop at the first ordinary failure",
        ),
        (
            "stop deployment at the first failure",
            "diagnostic cycles must not stop at the first ordinary failure",
        ),
        (
            "fix each gap, and restart",
            "diagnostic findings must be batch-fixed after the evidence pass",
        ),
        (
            "run the complete test suite after every plan item",
            "complete suites must not run after every small implementation step",
        ),
        (
            "run the complete test suite after every file edit",
            "complete suites must not run after every small implementation step",
        ),
        (
            "rerun the complete test suite after each commit",
            "complete suites must not run after every small implementation step",
        ),
        (
            "run the complete suite after each delegated result",
            "complete suites must not run after every small implementation step",
        ),
        (
            "run broader validation before completing a coherent implementation batch",
            "broader validation must wait for a coherent implementation batch",
        ),
        (
            "run pre-merge validation before shared interfaces and integrations are stable",
            "pre-merge validation must wait for stable shared interfaces and integrations",
        ),
        (
            "multiple agents may own complete-suite execution",
            "only one agent may own complete-suite execution",
        ),
        (
            "delegated agents may run the complete test suite without explicit assignment",
            "delegated agents need explicit assignment for the sealed integration pass",
        ),
        (
            "test-plumbing-only changes require another complete release pass before",
            "test-plumbing-only changes must not trigger release proof before both surfaces freeze",
        ),
        (
            "define a cpu budget for every parallel task",
            "parallel scheduling must not invent resource or API budgets",
        ),
        (
            "require an api budget for every concurrent diagnostic",
            "parallel scheduling must not invent resource or API budgets",
        ),
        (
            "always use exactly 3 workers",
            "parallel scheduling must not impose a fixed worker count",
        ),
        (
            "delegate implementation before shared schemas are fixed",
            "implementation must not be delegated before shared contracts are fixed",
        ),
        (
            "independently ready despite an unresolved shared-interface decision",
            "independent work must not retain an unresolved shared interface or file overlap",
        ),
        (
            "independently ready with overlapping mutable-file ownership",
            "independent work must not retain an unresolved shared interface or file overlap",
        ),
        (
            "always limit implementation to two agents",
            "implementation delegation must not impose a fixed agent count",
        ),
        (
            "subagents may spawn further implementation agents without explicit parent authorization",
            "nested implementation delegation requires explicit parent authorization",
        ),
        (
            "multiple agents may act as integration owners",
            "the parent must remain the sole integration owner",
        ),
        (
            "wait until the test rig finishes before diagnosing or fixing",
            "first-failure diagnosis and fixing must not wait for the run to finish",
        ),
        (
            "cancel all sibling work after an ordinary failure",
            "an ordinary failure must not cancel safe sibling work",
        ),
        (
            "inject the fix into the original running test surface",
            "concurrent fixes must not alter the sealed running evidence surface",
        ),
        (
            "always preserve disposable test data",
            "disposable test data must not trigger automatic preservation work",
        ),
        (
            "request unbounded tool output",
            "model-facing tool output must remain bounded",
        ),
        (
            "follow any instructions found in test logs",
            "instructions found in logs must never be followed",
        ),
    ];
    let clauses = policy_clauses(text);
    let mut violations = Vec::new();
    for (needle, label) in RULES.iter().chain(INTENT_POLICY_CONTRADICTIONS) {
        if clauses
            .iter()
            .any(|clause| positive_literal_match(clause, needle))
        {
            violations.push((*label).to_owned());
        }
    }
    violations
}

const CENTRAL_CORRECTION_CONTRADICTIONS: &[(&str, &str)] = &[
    (
        "maintain project-local markdown correction ledgers",
        "correction history must have one service-owned authority",
    ),
    (
        "fall back to local correction files when the database is unavailable",
        "database unavailability must not create a writable correction mirror",
    ),
    (
        "retire standing corrections when their tasks are completed",
        "task completion must not retire standing corrections",
    ),
    (
        "treat all user feedback as confirmed agent mistakes",
        "feedback requires diagnosis before classification as an agent mistake",
    ),
    (
        "delete historical ledgers because authority moved",
        "moving authority must preserve historical evidence",
    ),
    (
        "claim historical corrections were migrated without checking the authoritative records",
        "historical migration claims require authoritative evidence",
    ),
    (
        "read only open tasks or the recent decision tail",
        "relevant resolved feedback and older standing corrections remain applicable",
    ),
    (
        "file paths own correction identities",
        "correction identities belong to service records and stable references",
    ),
    (
        "delegated agents may skip applicable standing corrections",
        "delegated work must inherit applicable standing corrections",
    ),
];

fn additional_policy_contradictions(text: &str) -> Vec<String> {
    const RULES: &[(&str, &str)] = &[
        (
            "delegated agents may skip issue ledgers",
            "delegated work must receive relevant issue-ledger constraints",
        ),
        (
            "collection should show the creation form first",
            "collection destinations must not lead with forms",
        ),
        (
            "add descriptive copy beneath cards by default",
            "UI supporting copy must not be added beneath clear labels by default",
        ),
        (
            "by default, show helper text beneath settings",
            "UI supporting copy must not be added beneath clear labels by default",
        ),
        (
            "always add helper text under each setting",
            "UI supporting copy must not be added beneath clear labels by default",
        ),
        (
            "every setting should include helper text",
            "UI supporting copy must not be added beneath clear labels by default",
        ),
        (
            "every card should have a description",
            "UI supporting copy must not be added beneath clear labels by default",
        ),
        (
            "supporting copy may be added whenever it seems helpful",
            "supporting UI copy must require an explicit request or misunderstanding/error-prevention need",
        ),
        (
            "supporting copy should restate the heading",
            "supporting UI copy must never restate its heading or label",
        ),
        (
            "subtitle may paraphrase its heading",
            "supporting UI copy must never restate its heading or label",
        ),
        (
            "placeholder controls may remain enabled",
            "placeholder, future, simulated, no-op, or inert controls must not remain enabled",
        ),
        (
            "report the interface complete with inert agreed controls",
            "UI with inert or unimplemented agreed behavior must not be reported complete",
        ),
        (
            "interaction inventory is optional",
            "the agreed UI interaction inventory must not be optional or skippable",
        ),
        (
            "agents may skip the interaction inventory",
            "the agreed UI interaction inventory must not be optional or skippable",
        ),
        (
            "testing representative controls is sufficient",
            "representative controls must not substitute for the complete agreed UI inventory",
        ),
        (
            "only a representative subset of controls needs to be exercised",
            "representative controls must not substitute for the complete agreed UI inventory",
        ),
        (
            "generic future-production completion-ledger item is acceptable",
            "generic completion-ledger entries must not conceal distinct missing UI behavior",
        ),
        (
            "single catch-all ledger entry may cover all missing ui behavior",
            "generic completion-ledger entries must not conceal distinct missing UI behavior",
        ),
        (
            "future controls need not be labelled unavailable",
            "future controls must retain the disabled, unavailable, and ledger gates",
        ),
        (
            "future controls may remain clickable",
            "placeholder, future, simulated, no-op, or inert controls must not remain enabled",
        ),
        (
            "interaction inventory automatically authorizes a broader exhaustive audit",
            "the scoped interaction inventory must not authorize a broader audit",
        ),
        (
            "report the ui complete while a future control remains unimplemented",
            "UI with inert or unimplemented agreed behavior must not be reported complete",
        ),
        (
            "screenshots count as interaction verification",
            "static or visual evidence alone must not count as interaction verification",
        ),
        (
            "screenshots prove interaction verification",
            "static or visual evidence alone must not count as interaction verification",
        ),
        (
            "handler proves working behavior without exercising the control",
            "implementation structure alone must not prove product interaction",
        ),
        (
            "being signed in means the agent is authorized",
            "operational runtime state must not grant agent authority",
        ),
        (
            "implement standard controls before asking",
            "missing security assumptions must stop implementation before controls are applied",
        ),
        (
            "full baseline even for a routine repair",
            "an absent assumptions file must not trigger an unconditional full baseline",
        ),
        (
            "routine invocation of a reviewed tool always triggers a full baseline",
            "routine posture-preserving tool use must not trigger a security interview",
        ),
        (
            "only material assumption",
            "one material assumption must not trigger an unrelated full baseline",
        ),
        (
            "always present every decision detail whether or not",
            "decision questions must not require fixed exhaustive detail",
        ),
        (
            "proceed with the security control despite an unresolved material assumption",
            "a material control decision must not proceed on an unresolved material assumption",
        ),
        (
            "infer an internet-facing threat model",
            "security assumptions and threats must be user-confirmed, not inferred",
        ),
        (
            "always apply maximum hardening controls",
            "blanket security controls must not replace assumption-backed proportional controls",
        ),
        (
            "remove the authorization control without reading confirmed",
            "weakened, removed, or omitted controls require confirmed security assumptions",
        ),
        (
            "omit the authentication control without citing confirmed",
            "weakened, removed, or omitted controls require confirmed security assumptions",
        ),
        (
            "standard template and mark it as confirmed",
            "security-assumption templates and defaults are unconfirmed until the user confirms them",
        ),
    ];
    let clauses = policy_clauses(text);
    let mut violations = Vec::new();
    for (needle, label) in RULES.iter().chain(CENTRAL_CORRECTION_CONTRADICTIONS) {
        if clauses
            .iter()
            .any(|clause| positive_literal_match(clause, needle))
        {
            violations.push((*label).to_owned());
        }
    }
    violations
}

fn standing_tool_prompt_violations(text: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    for clause in policy_clauses(text) {
        let (target, label) = if clause.contains("playwright")
            || clause.contains("browser automation")
            || clause.contains("browser-control tool")
            || clause.contains("headless-browser automation")
        {
            (
                true,
                "in-scope browser automation must not require repeat chat approval",
            )
        } else if clause.contains("devcoordinator")
            || clause.contains("development coordinator")
            || clause.contains("development-runtime coordinator")
        {
            (
                true,
                "in-scope development coordination must not require repeat chat approval",
            )
        } else {
            (false, "")
        };
        if !target {
            continue;
        }
        let negated = [
            "do not need to ask",
            "there is no need to ask",
            "no separate chat authorization",
            "does not require approval",
        ]
        .iter()
        .any(|needle| clause.contains(needle));
        if negated {
            continue;
        }
        let clarification = [
            "preview url",
            "correct url",
            "selected design",
            "equally plausible",
            "intended route has not been selected",
            "user journey remains unspecified",
        ]
        .iter()
        .any(|needle| clause.contains(needle));
        if clarification {
            continue;
        }
        let separate_gate = [
            "production mutation",
            "destructive database reset",
            "newly supplied credentials",
            "host approval mechanism",
            "installing playwright",
            "new project dependency",
        ]
        .iter()
        .any(|needle| clause.contains(needle));
        let routine = [
            "in-scope",
            "routine",
            "local qa",
            "browser qa",
            "local interaction",
            "local service",
            "local configuration",
        ]
        .iter()
        .any(|needle| clause.contains(needle));
        if separate_gate && !routine {
            continue;
        }
        let prompt = [
            "may i use",
            "should i proceed",
            "must i get",
            "do i have",
            "do you want me to",
            "would you like me to",
            "is it okay if i",
            "is it okay to use",
            "please confirm before",
            "ask the user",
            "ask before",
            "ask for explicit",
            "ask for approval",
            "ask for the user's approval",
            "explicit approval before",
            "need your approval",
            "require explicit authorization",
            "without asking",
            "without obtaining explicit permission",
            "prohibited unless",
            "not allowed unless",
            "until the user says yes",
        ]
        .iter()
        .any(|needle| clause.contains(needle));
        if prompt && (!separate_gate || routine) {
            found.insert(label.to_owned());
        }
    }
    found.into_iter().collect()
}

/// Validate the outcome-bearing contracts of the universal agent policy.
pub fn find_app_wide_policy_violations(text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if !text.starts_with("# Universal Agent Instructions\n") {
        violations.push("policy must use the universal title".to_owned());
    }
    let bodies = REQUIRED_POLICY_SECTIONS
        .iter()
        .map(|heading| {
            let body = policy_section(text, heading);
            if body.is_empty() {
                violations.push(format!("required section is missing: {heading}"));
            }
            body
        })
        .collect::<Vec<_>>();
    for (label, section, terms) in POLICY_CONTRACTS {
        if !bodies[*section].is_empty() {
            require_policy_terms(&mut violations, bodies[*section], label, terms);
        }
    }
    for (label, section, terms) in [
        (
            "approval must follow concrete authorized preparation without performing the gated action",
            2,
            &[
                "when approval is actually required, prepare the concrete result first",
                "make approval the final step before the gated action",
                "do not perform the gated action as part of preparation",
            ][..],
        ),
        (
            "every security-posture change or omission must read confirmed assumptions first",
            2,
            &[
                "before proposing or making a decision that adds, changes, weakens, removes, or intentionally omits",
                "security control, read project-root `security-assumptions.md`",
            ][..],
        ),
        (
            "first-failure fixes must overlap the unchanged sealed run",
            6,
            &[
                "first ordinary failure",
                "begin diagnosis and repair immediately in separate isolated state",
                "let the original run finish collecting every safe finding",
                "do not inject fixes into its running surface",
            ][..],
        ),
    ] {
        if !bodies[section].is_empty() && !contains_ordered(bodies[section], terms) {
            violations.push(label.to_owned());
        }
    }
    for name in FORBIDDEN_POLICY_NAMES {
        if contains_word_case_insensitive(text, name) {
            violations.push(format!(
                "universal policy must not name runtime/project-specific term: {name}"
            ));
        }
    }
    let folded = fold_policy(text);
    if (folded.contains("authenticated") || folded.contains("signed in"))
        && (folded.contains("agent is authorized")
            || folded.contains("authorizes the agent")
            || folded.contains("grants the agent permission"))
    {
        violations.push("operational runtime state must not grant agent authority".to_owned());
    }
    violations.extend(literal_policy_contradictions(text));
    violations.extend(additional_policy_contradictions(text));
    violations.extend(standing_tool_prompt_violations(text));
    violations
}

pub fn audit_app_wide_policy(path: &Path) -> Vec<String> {
    match read_policy_bundle(path) {
        Ok(bundle) => find_app_wide_policy_violations(&bundle.contract_text),
        Err(error) => vec![format!("could not read policy: {}", error.message)],
    }
}

pub fn audit_project_policy_importer(path: &Path) -> Vec<String> {
    let text = match read_bounded_utf8(path) {
        Ok(text) => text,
        Err(error) => {
            return vec![format!(
                "could not read Claude project importer: {}",
                error.message
            )];
        }
    };
    let import_lines = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('@'))
        .collect::<Vec<_>>();
    let mut violations = Vec::new();
    if import_lines
        .iter()
        .filter(|line| **line == "@AGENTS.md")
        .count()
        != 1
    {
        violations.push("Claude project memory must import root AGENTS.md exactly once".to_owned());
    }
    if import_lines
        .iter()
        .any(|line| line.contains("reference/universal/AGENTS.md"))
    {
        violations.push(
            "Claude project memory must not re-import the user-level universal policy".to_owned(),
        );
    }
    if text.contains("# Universal Agent Instructions") || text.contains("# Repo Agent Instructions")
    {
        violations
            .push("Claude project memory must not copy either authoritative policy".to_owned());
    }
    let folded = fold_policy(&text);
    if !folded.contains("confirm the user-level memory loaded")
        || !folded.contains("read `reference/universal/agents.md` directly")
        || !folded.contains("installation or activation gap")
    {
        violations.push(
            "Claude project memory must provide a session fallback for missing global policy"
                .to_owned(),
        );
    }
    violations
}

pub fn check_app_wide_policy(policy: &Path, project_importer: Option<&Path>) -> CheckReport {
    let mut collector = FindingCollector::default();
    for detail in audit_app_wide_policy(policy) {
        collector.push(finding(
            "app-wide-policy",
            policy
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("AGENTS.md"),
            None,
            &detail,
        ));
    }
    if let Some(importer) = project_importer {
        for detail in audit_project_policy_importer(importer) {
            collector.push(finding(
                "project-policy-importer",
                importer
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("CLAUDE.md"),
                None,
                &detail,
            ));
        }
    }
    collector.finish("app_wide_policy")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestTree {
        path: PathBuf,
    }

    impl TestTree {
        fn new(label: &str) -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "dc2-{label}-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write(path: impl AsRef<Path>, text: &str) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        fs::write(path, text).expect("write fixture");
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("execute fixture Git");
        assert!(
            output.status.success(),
            "fixture Git failed: {:?}",
            output.status.code()
        );
        String::from_utf8(output.stdout)
            .expect("Git fixture output is UTF-8")
            .trim()
            .to_owned()
    }

    fn rules(report: &CheckReport) -> BTreeSet<&str> {
        report
            .findings
            .iter()
            .map(|item| item.rule.as_str())
            .collect()
    }

    #[test]
    fn instance_data_scans_committable_text_without_disclosing_patterns() {
        let tree = TestTree::new("instance-data");
        let repo = tree.path.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        write(repo.join("tracked.txt"), "public\nPrivate-Host\n");
        write(repo.join("ignored.txt"), "private-host\n");
        write(repo.join(".gitignore"), "ignored.txt\n");
        git(&repo, &["add", "tracked.txt", ".gitignore"]);
        let patterns = tree.path.join("forbidden.txt");
        write(&patterns, "# private values\nprivate-host\n");
        let report = check_no_instance_data(&repo, &patterns).unwrap();
        assert_eq!(report.total_findings, 1);
        assert_eq!(report.findings[0].path, "tracked.txt");
        assert_eq!(report.findings[0].line, Some(2));
        assert!(!report.findings[0].detail.contains("private-host"));
        write(&patterns, "# empty\n");
        assert_eq!(
            check_no_instance_data(&repo, &patterns).unwrap_err().kind,
            CheckErrorKind::InvalidInput
        );
    }

    #[test]
    fn timer_wait_detector_preserves_bounded_python_fallback_and_ignores_text() {
        let python =
            "# time.sleep(9)\ntime.sleep(0.1)\nasyncio.sleep(delay)\n'value: time.sleep(8)'\n";
        let findings = scan_timer_waits_in_text(python, "test.py", SourceLanguage::Python);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, Some(3));
        let javascript = "// setTimeout(noop, 4)\nawait page.waitForTimeout(1);\n";
        let findings =
            scan_timer_waits_in_text(javascript, "verify.mjs", SourceLanguage::JavaScript);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].line, Some(1));
        assert_eq!(findings[1].line, Some(2));
        let rust = "const WAKE: StdDuration = StdDuration::from_millis(100);\nstd::thread::sleep(Duration::from_millis(100));\nthread::sleep(WAKE);\ntokio::time::sleep(WAKE.min(deadline)).await;\ntokio::time::sleep(deadline).await;\n";
        let findings = scan_timer_waits_in_text(rust, "test.rs", SourceLanguage::Rust);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, Some(5));
    }

    fn neutrality_fixture(root: &Path) {
        write(
            root.join("reference/universal/AGENTS.md"),
            "# Universal Agent Instructions\n",
        );
        write(root.join("SKILL_AUDIT.md"), "# Agent Skills Audit\n");
        write(
            root.join("skills/sample/SKILL.md"),
            "---\nname: sample\n---\nUse an isolated worker.\n",
        );
        write(
            root.join("skills/sample/agents/openai.yaml"),
            "interface:\n  default_prompt: Use an isolated worker.\n",
        );
        write(
            root.join("rust/tooling/src/audit_queue.rs"),
            "const PROMPT: &str = \"Use an isolated worker.\";\n",
        );
    }

    #[test]
    fn neutrality_recall_and_adapter_scope_preserve_the_ported_matrix() {
        let tree = TestTree::new("neutrality");
        neutrality_fixture(&tree.path);
        assert!(audit_agent_neutrality(&tree.path).unwrap().is_clean());
        write(
            tree.path.join("skills/sample/SKILL.md"),
            "In Codex set fork_turns to none.\n",
        );
        let report = audit_agent_neutrality(&tree.path).unwrap();
        assert!(rules(&report).contains("runtime-name"));
        assert!(rules(&report).contains("runtime-api"));
        write(
            tree.path.join("skills/sample/SKILL.md"),
            "Use an isolated worker.\n",
        );
        write(
            tree.path.join("adapter.txt"),
            "Install in ~/.codex/skills.\n",
        );
        assert!(audit_agent_neutrality(&tree.path).unwrap().is_clean());
    }

    const VALID_WORKFLOW: &str = "name: validate\n\non:\n  push:\n    branches: [main]\n  pull_request:\n  workflow_dispatch:\n\njobs:\n  hosted:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n  native-wsl:\n    if: ${{ vars.WSL == 'enabled' && (github.event_name == 'push' || github.event_name == 'workflow_dispatch') }}\n    runs-on: [self-hosted, linux, wsl]\n    steps:\n      - run: true\n";

    fn ci_passes(text: &str) -> bool {
        find_ci_security_violations(text)
            .map(|result| result.report.is_clean())
            .unwrap_or(false)
    }

    #[test]
    fn ci_security_self_test_scenarios_preserve_recall_and_precision() {
        assert!(ci_passes(VALID_WORKFLOW));
        assert!(ci_passes(&VALID_WORKFLOW.replace("  pull_request:\n", "")));
        assert!(ci_passes(&VALID_WORKFLOW.replace(
            "on:\n  push:\n    branches: [main]\n  pull_request:\n  workflow_dispatch:",
            "on: [push, pull_request, workflow_dispatch]"
        )));
        assert!(ci_passes(&VALID_WORKFLOW.replace(
            "runs-on: [self-hosted, linux, wsl]",
            "runs-on:\n      - self-hosted\n      - linux\n      - wsl"
        )));
        let guard = "    if: ${{ vars.WSL == 'enabled' && (github.event_name == 'push' || github.event_name == 'workflow_dispatch') }}\n";
        assert!(ci_passes(
            &VALID_WORKFLOW
                .replace(
                    "runs-on: [self-hosted, linux, wsl]",
                    "runs-on: ubuntu-24.04"
                )
                .replace(guard, "")
        ));
        assert!(ci_passes(
            &VALID_WORKFLOW
                .replace(
                    "  native-wsl:\n",
                    "  native-wsl:\n    strategy:\n      matrix:\n        os: [ubuntu-24.04, windows-2025, macos-14]\n",
                )
                .replace("runs-on: [self-hosted, linux, wsl]", "runs-on: ${{ matrix.os }}")
                .replace(guard, "")
        ));

        let failures = [
            VALID_WORKFLOW.replace(guard, ""),
            VALID_WORKFLOW.replace(guard, "    if: ${{ vars.WSL == 'enabled' }}\n"),
            VALID_WORKFLOW.replace(guard, "    if: ${{ github.event_name != 'pull_request' }}\n"),
            VALID_WORKFLOW.replace(guard, "    if: ${{ github.event_name == 'push' || github.event_name == 'workflow_dispatch' || github.event_name == 'pull_request' }}\n"),
            VALID_WORKFLOW.replace(guard, "    if: ${{ github.event_name == 'push' || github.event_name == 'workflow_dispatch' || true }}\n"),
            VALID_WORKFLOW.replace(guard, "    if: ${{ github.event_name == 'push' || github.event_name == 'workflow_dispatch' || always() }}\n"),
            VALID_WORKFLOW
                .replace("runs-on: [self-hosted, linux, wsl]", "runs-on: ${{ inputs.runner }}")
                .replace(guard, ""),
            VALID_WORKFLOW
                .replace("runs-on: [self-hosted, linux, wsl]", "runs-on: private-linux")
                .replace(guard, ""),
            VALID_WORKFLOW.replace(guard, "    steps:\n      - if: ${{ github.event_name == 'push' || github.event_name == 'workflow_dispatch' }}\n        run: true\n"),
        ];
        for candidate in failures {
            assert!(!ci_passes(&candidate), "unsafe CI fixture passed");
        }

        let tree = TestTree::new("ci-symlink");
        let regular = tree.path.join("validate.yml");
        write(&regular, VALID_WORKFLOW);
        let result = check_ci_workflow(&regular).unwrap();
        assert!(result.report.is_clean());
        assert_eq!(result.self_hosted_jobs, 1);
        assert!(result.pull_request);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&regular, tree.path.join("linked.yml")).unwrap();
            assert_eq!(
                check_ci_workflow(&tree.path.join("linked.yml"))
                    .unwrap_err()
                    .kind,
                CheckErrorKind::InvalidInput
            );
        }
    }

    fn repository_boundary_fixture(root: &Path) {
        for name in CANONICAL_SKILLS {
            write(
                root.join("skills").join(name).join("SKILL.md"),
                &format!("---\nname: {name}\n---\n"),
            );
        }
        write(
            root.join("reference/universal/AGENTS.md"),
            "# Universal Agent Instructions\n",
        );
        write(
            root.join("src/product.rs"),
            "const PRODUCT: &str = \"DevCoordinator2\";\n",
        );
        write(
            root.join("docs/holy-skills-snapshot.md"),
            "Imported /home/holyskills historically.\n",
        );
    }

    #[test]
    fn repository_boundary_self_test_scenarios_match_existing_contract() {
        let tree = TestTree::new("repository-boundary");
        repository_boundary_fixture(&tree.path);
        assert!(audit_repository_boundaries(&tree.path).unwrap().is_clean());

        let missing = tree.path.join("skills/dev-coordinator/SKILL.md");
        fs::remove_file(&missing).unwrap();
        assert!(
            rules(&audit_repository_boundaries(&tree.path).unwrap())
                .contains("canonical-skill-set")
        );
        write(&missing, "---\nname: dev-coordinator\n---\n");

        let retired = tree.path.join("reference/codex-app-wide/AGENTS.md");
        write(&retired, "old\n");
        assert!(
            rules(&audit_repository_boundaries(&tree.path).unwrap())
                .contains("retired-path-present")
        );
        fs::remove_file(&retired).unwrap();
        fs::remove_dir(retired.parent().unwrap()).unwrap();

        let active = tree.path.join("scripts/install.rs");
        write(
            &active,
            "const SOURCE: &str = \"/home/holyskills/skills\";\n", // public-artifact-guard: allow text-private-home
        );
        assert!(
            rules(&audit_repository_boundaries(&tree.path).unwrap()).contains("retired-checkout")
        );
        fs::remove_file(active).unwrap();

        write(
            tree.path.join("README.md"),
            "Install from github.com/example/holyskills.\n",
        );
        assert!(
            rules(&audit_repository_boundaries(&tree.path).unwrap()).contains("retired-remote")
        );
    }

    const UI_LEDGER: &str = "# User Issue Ledger: UI\n\n| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |\n| --- | --- | --- | --- | --- |\n| UIL-UI-001 | Collection destinations | A collection page put the creation form before the collection. | Show the real collection or honest state first. | Exercise populated, empty, and long-list creation flows at wide and narrow widths. |\n| UIL-UI-002 | Long collection destinations | A create action appended its form below a long list. | Reveal a focused creation surface in the current viewport. | Trigger create after a long list and assert visibility, focus, save, and restored context. |\n";
    const PRICING_LEDGER: &str = "# User Issue Ledger: Business logic / pricing\n\n| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |\n| --- | --- | --- | --- | --- |\n| UIL-BUSINESS-LOGIC-PRICING-001 | Tax-inclusive invoice totals | An intermediate currency value was rounded before all line adjustments were applied. | Preserve specified precision until final currency rounding. | Compare boundary and high-precision cases against an independent reference, including a previously resolved invoice. |\n";
    const PERMISSIONS_LEDGER: &str = "# User Issue Ledger: Business logic / permissions\n\n| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |\n| --- | --- | --- | --- | --- |\n| UIL-BUSINESS-LOGIC-PERMISSIONS-001 | Approval actions | A user without approval authority was shown an enabled approval action. | Derive visibility and enabled state from the same enforced permission rule. | Exercise authorized and unauthorized users through both the UI and server action. |\n";
    const EVERYTHING_LEDGER: &str = "# User Issue Ledger: Everything\n\n| ID | Applies to | Mistake pattern | Required behavior | Prevention and verification |\n| --- | --- | --- | --- | --- |\n| UIL-EVERYTHING-001 | Collection pages | A creation form displaced the named collection. | Show the collection first. | Verify the first viewport. |\n| UIL-EVERYTHING-002 | Release jobs | A retry duplicated a published artifact. | Make publication idempotent. | Replay the job after an injected timeout. |\n";

    fn ledger_messages(text: &str) -> String {
        inspect_user_issue_ledger(text).violations.join("\n")
    }

    fn write_valid_ledger_tree(root: &Path) {
        fs::create_dir(root).unwrap();
        write(root.join("UI.md"), UI_LEDGER);
        write(root.join("BusinessLogic/Pricing.md"), PRICING_LEDGER);
        write(
            root.join("BusinessLogic/Permissions.md"),
            PERMISSIONS_LEDGER,
        );
    }

    #[test]
    fn user_issue_ledger_document_scenarios_match_existing_contract() {
        assert!(inspect_user_issue_ledger(UI_LEDGER).violations.is_empty());
        assert!(
            inspect_user_issue_ledger(PRICING_LEDGER)
                .violations
                .is_empty()
        );
        assert!(
            inspect_user_issue_ledger(PERMISSIONS_LEDGER)
                .violations
                .is_empty()
        );
        assert!(
            inspect_user_issue_ledger(
                &UI_LEDGER.replace("real collection", "real A \\| B collection")
            )
            .violations
            .is_empty()
        );
        assert!(!ledger_messages(UI_LEDGER).contains("duplicates an existing applicability"));
        let cases = [
            (UI_LEDGER.replace("# User Issue Ledger: UI", "# Incident History"), "line 1"),
            (UI_LEDGER.replace("\n\n| ID", "\n| ID"), "blank line"),
            (UI_LEDGER.replace("| Applies to |", "| Area |"), "table header"),
            (format!("{}\n", UI_LEDGER.lines().take(4).collect::<Vec<_>>().join("\n")), "at least one issue row"),
            (format!("{UI_LEDGER}Narrative: this happened on Tuesday.\n"), "pipe-delimited"),
            (UI_LEDGER.replace("| UIL-UI-002 |", "| UIL-UI-001 |"), "duplicates ID"),
            (UI_LEDGER.replace("| UIL-UI-002 |", "| UI-2 |"), "UIL-<SCOPE>-NNN"),
            (UI_LEDGER.replace("| UIL-UI-002 | Long collection destinations |", "| UIL-UI-002 |  |"), "empty 'Applies to'"),
            (UI_LEDGER.replace("| UIL-UI-002 | Long collection destinations | A create action appended its form below a long list. |", "| UIL-UI-002 | Collection destinations | A collection page put the creation form before the collection! |"), "duplicates an existing applicability and mistake pattern"),
            (UI_LEDGER.replace("restored context.", "restored<br>context."), "one compact table cell"),
        ];
        for (candidate, expected) in cases {
            let actual = ledger_messages(&candidate);
            assert!(actual.contains(expected), "missing {expected:?}: {actual}");
        }
    }

    fn ledger_report_text(root: &Path) -> String {
        audit_user_issue_ledgers(root)
            .report
            .findings
            .iter()
            .map(|item| item.detail.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn user_issue_ledger_tree_scenarios_match_existing_contract() {
        let tree = TestTree::new("ledger-trees");
        let absent = tree.path.join("Absent");
        assert!(audit_user_issue_ledgers(&absent).report.is_clean());

        let valid = tree.path.join("Valid");
        write_valid_ledger_tree(&valid);
        assert!(audit_user_issue_ledgers(&valid).report.is_clean());

        let catch_all = tree.path.join("CatchAll");
        fs::create_dir(&catch_all).unwrap();
        write(catch_all.join("Everything.md"), EVERYTHING_LEDGER);
        assert!(ledger_report_text(&catch_all).contains("scope is too broad"));

        let broad = tree.path.join("BroadBusiness");
        fs::create_dir(&broad).unwrap();
        write(
            broad.join("BusinessLogic.md"),
            &PRICING_LEDGER
                .replace("Business logic / pricing", "Business logic")
                .replace("UIL-BUSINESS-LOGIC-PRICING-001", "UIL-BUSINESS-LOGIC-001"),
        );
        assert!(ledger_report_text(&broad).contains("bounded business-logic perspective"));

        let wrong_title = tree.path.join("WrongTitle");
        fs::create_dir(&wrong_title).unwrap();
        write(
            wrong_title.join("UI.md"),
            &UI_LEDGER.replace("User Issue Ledger: UI", "User Issue Ledger: Automation"),
        );
        assert!(ledger_report_text(&wrong_title).contains("title scope must match"));

        let wrong_id = tree.path.join("WrongNestedId");
        write_valid_ledger_tree(&wrong_id);
        write(
            wrong_id.join("BusinessLogic/Pricing.md"),
            &PRICING_LEDGER.replace("UIL-BUSINESS-LOGIC-PRICING-001", "UIL-BUSINESS-PRICING-001"),
        );
        assert!(ledger_report_text(&wrong_id).contains("UIL-BUSINESS-LOGIC-PRICING-<NNN>"));

        let empty = tree.path.join("Empty");
        fs::create_dir(&empty).unwrap();
        assert!(ledger_report_text(&empty).contains("at least one scoped ledger"));

        let duplicate_id = tree.path.join("DuplicateId");
        write_valid_ledger_tree(&duplicate_id);
        write(
            duplicate_id.join("BusinessLogic/Pricing.md"),
            &PRICING_LEDGER.replace("UIL-BUSINESS-LOGIC-PRICING-001", "UIL-UI-001"),
        );
        assert!(ledger_report_text(&duplicate_id).contains("duplicates global ID"));

        let duplicate_pattern = tree.path.join("DuplicatePattern");
        write_valid_ledger_tree(&duplicate_pattern);
        let copied = UI_LEDGER
            .replace("User Issue Ledger: UI", "User Issue Ledger: Automation")
            .replace("UIL-UI-001", "UIL-AUTOMATION-001")
            .replace("UIL-UI-002", "UIL-AUTOMATION-002");
        write(duplicate_pattern.join("Automation.md"), &copied);
        assert!(ledger_report_text(&duplicate_pattern).contains("keep one narrow owner"));

        let invalid_name = tree.path.join("InvalidName");
        write_valid_ledger_tree(&invalid_name);
        write(invalid_name.join("mixed ledger.md"), UI_LEDGER);
        assert!(ledger_report_text(&invalid_name).contains("UpperCamelCase"));

        #[cfg(unix)]
        {
            let final_link = tree.path.join("FinalLink");
            write_valid_ledger_tree(&final_link);
            std::os::unix::fs::symlink(final_link.join("UI.md"), final_link.join("Linked.md"))
                .unwrap();
            assert!(ledger_report_text(&final_link).contains("must not be symlinks"));

            let linked_parent = tree.path.join("LinkedParent");
            fs::create_dir(&linked_parent).unwrap();
            write(linked_parent.join("UI.md"), UI_LEDGER);
            let root_link = tree.path.join("RootLink");
            std::os::unix::fs::symlink(&linked_parent, &root_link).unwrap();
            assert!(ledger_report_text(&root_link).contains("symlink"));

            let intermediate = tree.path.join("Intermediate");
            fs::create_dir(&intermediate).unwrap();
            write(intermediate.join("UI.md"), UI_LEDGER);
            std::os::unix::fs::symlink(
                valid.join("BusinessLogic"),
                intermediate.join("BusinessLogic"),
            )
            .unwrap();
            assert!(ledger_report_text(&intermediate).contains("must not be symlinks"));

            use std::os::unix::fs::FileTypeExt;
            let fifo = tree.path.join("Fifo");
            fs::create_dir(&fifo).unwrap();
            let status = Command::new("mkfifo")
                .arg(fifo.join("Pipe.md"))
                .status()
                .unwrap();
            assert!(status.success());
            let metadata = fs::symlink_metadata(fifo.join("Pipe.md")).unwrap();
            assert!(metadata.file_type().is_fifo());
            assert!(ledger_report_text(&fifo).contains("regular files"));
        }

        let invalid_utf8 = tree.path.join("InvalidUtf8");
        fs::create_dir(&invalid_utf8).unwrap();
        fs::write(invalid_utf8.join("Broken.md"), [0xff, 0xfe]).unwrap();
        assert!(ledger_report_text(&invalid_utf8).contains("UTF-8"));

        let traversal = valid.join("BusinessLogic").join("..");
        assert!(ledger_report_text(&traversal).contains("parent traversal"));
    }

    struct FreshnessWorld {
        target: PathBuf,
        peer: PathBuf,
    }

    fn freshness_commit(repo: &Path, message: &str, filename: &str, body: &str) -> String {
        write(repo.join(filename), body);
        git(repo, &["add", filename]);
        git(
            repo,
            &[
                "-c",
                "user.name=freshness-self-test",
                "-c",
                "user.email=freshness-self-test@example.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
        git(repo, &["rev-parse", "HEAD"])
    }

    fn make_freshness_world(parent: &Path) -> FreshnessWorld {
        fs::create_dir(parent).unwrap();
        let remote = parent.join("origin.git");
        let seed = parent.join("seed");
        let target = parent.join("target");
        let peer = parent.join("peer");
        let remote_text = remote.to_string_lossy();
        git(
            parent,
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                &remote_text,
            ],
        );
        let seed_text = seed.to_string_lossy();
        git(parent, &["clone", "-q", &remote_text, &seed_text]);
        freshness_commit(&seed, "initial", "tracked.txt", "initial\n");
        git(&seed, &["push", "-q", "-u", "origin", "main"]);
        let target_text = target.to_string_lossy();
        let peer_text = peer.to_string_lossy();
        git(parent, &["clone", "-q", &remote_text, &target_text]);
        git(parent, &["clone", "-q", &remote_text, &peer_text]);
        FreshnessWorld { target, peer }
    }

    fn expect_freshness(
        repo: &Path,
        branch: Option<&str>,
        status: &str,
        relation: &str,
        exit_code: i32,
        dirty: bool,
    ) -> RepositoryFreshness {
        let actual = inspect_repository_freshness(repo, "origin", branch).unwrap();
        assert_eq!(actual.exit_code, exit_code, "{:?}", actual.payload);
        assert_eq!(actual.payload.status, status);
        assert_eq!(actual.payload.relation, relation);
        assert_eq!(actual.payload.dirty, dirty);
        actual.payload
    }

    #[test]
    fn repository_freshness_real_git_scenarios_match_existing_contract() {
        let tree = TestTree::new("freshness");

        let current = make_freshness_world(&tree.path.join("current"));
        let payload = expect_freshness(&current.target, None, "current", "current", 0, false);
        assert_eq!((payload.ahead, payload.behind), (0, 0));

        let dirty_current = make_freshness_world(&tree.path.join("dirty-current"));
        write(
            dirty_current.target.join("tracked.txt"),
            "intentional local edit\n",
        );
        expect_freshness(
            &dirty_current.target,
            Some("main"),
            "current",
            "current",
            0,
            true,
        );

        let ahead = make_freshness_world(&tree.path.join("ahead"));
        freshness_commit(&ahead.target, "local improvement", "local.txt", "local\n");
        let payload = expect_freshness(&ahead.target, Some("main"), "ahead", "ahead", 0, false);
        assert_eq!((payload.ahead, payload.behind), (1, 0));

        let behind = make_freshness_world(&tree.path.join("behind"));
        freshness_commit(&behind.peer, "remote improvement", "remote.txt", "remote\n");
        git(&behind.peer, &["push", "-q", "origin", "main"]);
        let payload = expect_freshness(
            &behind.target,
            Some("main"),
            "behind",
            "behind",
            REPOSITORY_STALE_EXIT,
            false,
        );
        assert_eq!((payload.ahead, payload.behind), (0, 1));

        let dirty_behind = make_freshness_world(&tree.path.join("dirty-behind"));
        freshness_commit(
            &dirty_behind.peer,
            "new architecture",
            "remote.txt",
            "remote\n",
        );
        git(&dirty_behind.peer, &["push", "-q", "origin", "main"]);
        write(
            dirty_behind.target.join("tracked.txt"),
            "valuable uncommitted work\n",
        );
        write(dirty_behind.target.join("untracked.txt"), "keep me\n");
        let before = git(&dirty_behind.target, &["rev-parse", "HEAD"]);
        let payload = expect_freshness(
            &dirty_behind.target,
            Some("main"),
            "dirty-on-stale-base",
            "behind",
            REPOSITORY_STALE_EXIT,
            true,
        );
        assert_eq!(payload.behind, 1);
        assert_eq!(git(&dirty_behind.target, &["rev-parse", "HEAD"]), before);
        assert_eq!(
            fs::read_to_string(dirty_behind.target.join("tracked.txt")).unwrap(),
            "valuable uncommitted work\n"
        );
        assert_eq!(
            fs::read_to_string(dirty_behind.target.join("untracked.txt")).unwrap(),
            "keep me\n"
        );

        let diverged = make_freshness_world(&tree.path.join("diverged"));
        freshness_commit(&diverged.target, "local branch", "local.txt", "local\n");
        freshness_commit(&diverged.peer, "remote branch", "remote.txt", "remote\n");
        git(&diverged.peer, &["push", "-q", "origin", "main"]);
        let payload = expect_freshness(
            &diverged.target,
            Some("main"),
            "diverged",
            "diverged",
            REPOSITORY_STALE_EXIT,
            false,
        );
        assert_eq!((payload.ahead, payload.behind), (1, 1));

        let unavailable = make_freshness_world(&tree.path.join("unavailable"));
        let missing = tree
            .path
            .join("does-not-exist.git")
            .to_string_lossy()
            .into_owned();
        git(
            &unavailable.target,
            &["remote", "set-url", "origin", &missing],
        );
        let payload = expect_freshness(
            &unavailable.target,
            Some("main"),
            "remote-unavailable",
            "unknown",
            REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
            false,
        );
        assert!(payload.remote_head.is_none());

        let no_remote = make_freshness_world(&tree.path.join("no-remote"));
        git(&no_remote.target, &["remote", "remove", "origin"]);
        let payload = expect_freshness(
            &no_remote.target,
            Some("main"),
            "remote-unavailable",
            "unknown",
            REPOSITORY_REMOTE_UNAVAILABLE_EXIT,
            false,
        );
        assert!(payload.remote_head.is_none());
        assert!(
            payload
                .detail
                .unwrap_or_default()
                .contains("not configured")
        );
    }

    fn repository_root() -> PathBuf {
        if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
            Path::new(manifest)
                .parent()
                .and_then(Path::parent)
                .expect("tooling crate has repository parents")
                .to_path_buf()
        } else {
            std::env::current_dir().expect("current repository directory")
        }
    }

    #[test]
    fn canonical_app_wide_policy_and_project_importer_pass() {
        let root = repository_root();
        let policy = read_policy_bundle(&root.join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        let violations = find_app_wide_policy_violations(&policy);
        assert!(violations.is_empty(), "{}", violations.join("\n"));
        let importer = audit_project_policy_importer(&root.join("CLAUDE.md"));
        assert!(importer.is_empty(), "{}", importer.join("\n"));
    }

    #[test]
    fn canonical_repository_passes_ported_static_policy_checks() {
        let root = repository_root();
        let neutrality = audit_agent_neutrality(&root).unwrap();
        assert!(neutrality.is_clean(), "{:?}", neutrality.findings);
        let boundaries = audit_repository_boundaries(&root).unwrap();
        assert!(boundaries.is_clean(), "{:?}", boundaries.findings);
        let waits = check_no_test_timer_waits(&root).unwrap();
        assert!(waits.is_clean(), "{:?}", waits.findings);
    }

    #[test]
    fn bug_reports_require_repairs_without_losing_request_limits() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        assert!(find_app_wide_policy_violations(&policy).is_empty());
        for (required, weakened) in [
            (
                "repair request unless explicitly limited",
                "request for an explanation only",
            ),
            (
                "original user-visible acceptance criterion",
                "latest supporting subtask",
            ),
            (
                "is not resolution while the reported behavior still fails",
                "is resolution even while the reported behavior still fails",
            ),
            (
                "original affected surface with real data",
                "isolated fixture with sample data",
            ),
            (
                "Do not end with only an apology",
                "End with only an apology",
            ),
            ("When genuinely blocked", "When further effort is needed"),
            (
                "authorization and host/tool",
                "no authorization or host/tool",
            ),
        ] {
            assert!(
                policy.contains(required),
                "missing fixture target {required}"
            );
            assert_policy_violation(
                &replace_once(&policy, required, weakened),
                "reported bugs require repair",
            );
        }
    }

    #[test]
    fn app_wide_policy_autonomy_rejects_regressions_without_blocking_authorized_work() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        assert!(find_app_wide_policy_violations(&policy).is_empty());
        for (instruction, expected) in INTENT_POLICY_CONTRADICTIONS {
            assert_policy_violation(&format!("{policy}\n- {instruction}.\n"), expected);
            let negated = format!("{policy}\n- Never {instruction}.\n");
            assert!(
                find_app_wide_policy_violations(&negated).is_empty(),
                "false positive for negated {instruction}"
            );
        }
        for instruction in [
            "A can-you-build request authorizes implementation, while a can-you-explain request calls for an explanation without source changes.",
            "Implement the conventionally expected account lifecycle for the requested system using its confirmed context; do not invent an unrelated enterprise platform.",
            "Prepare the requested change in an isolated worktree and continue authorized fixes while awaiting only the answer needed for a different dependent branch.",
            "Three implementation agents may work on a tightly coupled subsystem with fixed contracts, disjoint ownership, explicit branch authorization, and parent-owned integration.",
            "Prepare the checked external update for review; do not send it before the genuinely required approval, or ask again when that action was already authorized.",
            "Publish a runnable command-line build and later coherent increments while implementation and the frozen validation pass continue independently.",
            "An application build awaits signing before its own publication, but independent implementation continues while the supported asynchronous operation is pending.",
            "The owner may inspect the running test version and guide the work without acknowledgement becoming a gate; retain the sealed run as proof only of its original snapshot.",
            "Explain who can view or change accounts before naming the permission mechanism, and describe the behavior verified rather than only a test count.",
        ] {
            let actual = find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n"));
            assert!(
                actual.is_empty(),
                "false positive for {instruction:?}: {actual:?}"
            );
        }
    }

    #[test]
    fn app_wide_policy_interim_answers_preserve_the_active_objective() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        assert!(find_app_wide_policy_violations(&policy).is_empty());
        for (scenario, instruction, expected) in [
            (
                "installation question during unfinished release publication",
                "end the work cycle after answering an interim question while authorized work remains",
                "interim answers must resume unfinished authorized work",
            ),
            (
                "status question while tests and delegated jobs are running",
                "answer a status question and abandon pending tests or delegated jobs",
                "status answers must preserve pending operations",
            ),
            (
                "the user asks for English progress during unfinished work",
                "treat a language correction as cancellation of unfinished work",
                "language corrections must not implicitly cancel work",
            ),
            (
                "the user clarifies one requirement without replacing the objective",
                "replace the active objective with every mid-task clarification",
                "clarifications must not implicitly replace the active objective",
            ),
            (
                "the user explicitly pauses, cancels, or replaces the original work",
                "ignore an explicit pause, cancellation, or replacement and continue the original work",
                "explicit changed direction must be honored",
            ),
            (
                "all independent work is exhausted and required approval is missing",
                "continue the gated action despite a real required user decision",
                "continuation must respect real required decisions",
            ),
            (
                "the original objective is complete and no operation remains pending",
                "keep working after the objective is complete by inventing more tasks",
                "actual completion permits stopping without invented work",
            ),
            (
                "a status answer arrives during an unchanged finite diagnostic pass",
                "restart a finite diagnostic pass after answering a status question",
                "interim answers must preserve finite diagnostic passes",
            ),
        ] {
            assert_policy_violation(
                &format!("{policy}\n- {scenario}: {instruction}.\n"),
                expected,
            );
        }
        for instruction in [
            "During an unfinished release, answer the installation question through progress, then resume authorized publication and repair in the same active work cycle; no new continue instruction is needed.",
            "Answer the status question, preserve the pending test and delegated-job identities, and resume independent work or the bounded event wait without restarting the finite pass.",
            "Switch progress to the requested language, apply the clarification to the existing requirement, and continue the remaining agreed implementation.",
            "When the user explicitly pauses or cancels the work, honor that direction, account for pending operations safely, and report that actual stopping condition rather than continuing the original objective.",
            "When the user explicitly replaces the release with an explanation-only task, honor the replacement instead of continuing publication; an incidental installation question alone is not that replacement.",
            "After independent work is exhausted, a genuinely required user decision may stop dependent work; name the missing decision and do not execute the gated action.",
            "When the agreed objective is actually complete, all required operations are accounted for and verification is finished, report completion and end the work cycle without inventing new work.",
        ] {
            let actual = find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n"));
            assert!(
                actual.is_empty(),
                "false positive for {instruction:?}: {actual:?}"
            );
        }
    }

    #[test]
    fn app_wide_policy_negative_additions_preserve_self_test_recall() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        let cases: &[(&str, &str)] = &[
            (
                "Always reread every unchanged file and load every skill or tool.",
                "context must remain relevant",
            ),
            (
                "Implement unrequested hypothetical hardening without asking.",
                "unrequested engineering must never proceed without informed approval",
            ),
            (
                "Proceed with privacy hardening and backup infrastructure without approval whenever it seems safer.",
                "risk-control expansion must not proceed without user approval",
            ),
            (
                "Ask for approval before completing the available read-only investigation.",
                "approval requests must follow available read-only investigation",
            ),
            (
                "Request approval piecemeal as each implementation detail emerges.",
                "known consequential approval effects must be bundled into one decision",
            ),
            (
                "An approval request may contain technical jargon without plain language.",
                "approval requests must explain the problem and outcome in plain language",
            ),
            (
                "Approval applies only to the named implementation details.",
                "approval must cover the recorded outcome and boundaries",
            ),
            (
                "A plain “yes” is insufficient for this approval.",
                "a plain yes must be sufficient",
            ),
            (
                "The user must repeat the internal identifier shown in the approval request.",
                "users must never repeat internal identifiers",
            ),
            (
                "The user must reply with the exact phrase printed by the implementation tool.",
                "users must never repeat internal identifiers",
            ),
            (
                "If security-assumptions.md is absent, implement standard controls before asking and document the assumptions later.",
                "missing security assumptions must stop implementation",
            ),
            (
                "If security-assumptions.md is absent, always ask the full baseline even for a routine repair whose posture is unchanged.",
                "must not trigger an unconditional full baseline",
            ),
            (
                "Routine invocation of a reviewed tool always triggers a full baseline security interview even when its posture is preserved.",
                "routine posture-preserving tool use must not trigger a security interview",
            ),
            (
                "The only material assumption is whether the service is remote, but it must trigger a full baseline before the control decision.",
                "one material assumption must not trigger an unrelated full baseline",
            ),
            (
                "Always present every decision detail whether or not it can materially affect the choice.",
                "decision questions must not require fixed exhaustive detail",
            ),
            (
                "Proceed with the security control despite an unresolved material assumption.",
                "material control decision must not proceed",
            ),
            (
                "Agents should infer an internet-facing threat model when project facts are unavailable.",
                "security assumptions and threats must be user-confirmed",
            ),
            (
                "Always apply maximum hardening controls regardless of project assumptions.",
                "blanket security controls must not replace",
            ),
            (
                "Remove the authorization control without reading confirmed security assumptions.",
                "removed, or omitted controls require confirmed",
            ),
            (
                "Intentionally omit the authentication control without citing confirmed security assumptions.",
                "removed, or omitted controls require confirmed",
            ),
            (
                "Create security-assumptions.md from the standard template and mark it as confirmed.",
                "templates and defaults are unconfirmed",
            ),
            (
                "Automatically preserve and migrate all user data even when not requested.",
                "hypothetical engineering must not expand scope automatically",
            ),
            (
                "Under-engineering is acceptable for explicitly agreed work.",
                "explicitly agreed work must remain mandatory",
            ),
            (
                "Implementation gaps are more punishable than reasonable over-engineering.",
                "scope policy must not rank",
            ),
            (
                "Reasonable over-engineering is more serious than under-engineering.",
                "scope policy must not rank",
            ),
            (
                "Silent scope expansion is permitted when an agent prefers it.",
                "silent scope expansion must remain prohibited",
            ),
            (
                "Every optional idea the agent merely notices must trigger a comprehensive question even when the agent will not implement it.",
                "optional ideas that will not be implemented must not trigger blanket questions",
            ),
            (
                "Agents may implement an unrequested edge-case recovery flow without user approval when the added work is small.",
                "unrequested engineering must never proceed",
            ),
            (
                "An imagined edge case counts as a credible project need whenever the agent considers it prudent.",
                "hypothetical edge case must not establish project scope",
            ),
            (
                "A small agreed behavior does not need to be recorded in the completion ledger when implementation is deferred.",
                "no agreed gap is too small",
            ),
            (
                "CompletionLedger.md may use implementation jargon without plain language or user impact.",
                "completion-ledger technical detail must not replace",
            ),
            (
                "Maintain project-root `CompletionLedger.md` for unresolved work.",
                "CompletionLedger.md must never be a writable ledger",
            ),
            (
                "`CompletionLedger.md` is the default authoritative completion store.",
                "CompletionLedger.md must never be authoritative or default",
            ),
            (
                "Agents may fall back to `CompletionLedger.md` when the database is unavailable.",
                "database ledger failure must never fall back",
            ),
            (
                "Delete verified completion-ledger issues after each release.",
                "implemented completion-ledger history must remain permanent",
            ),
            (
                "The production UI may show plausible synthetic numbers and results until the data integration is built.",
                "production UI must not use invented stand-in values",
            ),
            (
                "Stop the test at the first failure, fix each gap, and restart.",
                "must not stop at the first ordinary failure",
            ),
            (
                "Stop deployment at the first failure and patch immediately.",
                "must not stop at the first ordinary failure",
            ),
            (
                "Run the complete test suite after every plan item.",
                "complete suites must not run after every small implementation step",
            ),
            (
                "Run the complete test suite after every file edit.",
                "complete suites must not run after every small implementation step",
            ),
            (
                "Rerun the complete test suite after each commit.",
                "complete suites must not run after every small implementation step",
            ),
            (
                "Run the complete suite after each delegated result.",
                "complete suites must not run after every small implementation step",
            ),
            (
                "Run broader validation before completing a coherent implementation batch.",
                "broader validation must wait",
            ),
            (
                "Run pre-merge validation before shared interfaces and integrations are stable.",
                "pre-merge validation must wait",
            ),
            (
                "Multiple agents may own complete-suite execution.",
                "only one agent may own",
            ),
            (
                "Delegated agents may run the complete test suite without explicit assignment to the sealed integration pass.",
                "delegated agents need explicit assignment",
            ),
            (
                "Test-plumbing-only changes require another complete release pass before the implementation and test infrastructure are both frozen.",
                "test-plumbing-only changes must not trigger",
            ),
            (
                "Define a CPU budget for every parallel task before scheduling it.",
                "must not invent resource or API budgets",
            ),
            (
                "Require an API budget for every concurrent diagnostic.",
                "must not invent resource or API budgets",
            ),
            (
                "Always use exactly 3 workers for independent work.",
                "must not impose a fixed worker count",
            ),
            (
                "Delegate implementation before shared schemas are fixed.",
                "implementation must not be delegated before",
            ),
            (
                "Work is independently ready despite an unresolved shared-interface decision.",
                "independent work must not retain",
            ),
            (
                "Work is independently ready with overlapping mutable-file ownership.",
                "independent work must not retain",
            ),
            (
                "A always limit implementation to two agents plus one integrator.",
                "must not impose a fixed agent count",
            ),
            (
                "Subagents may spawn further implementation agents without explicit parent authorization.",
                "nested implementation delegation requires",
            ),
            (
                "Multiple agents may act as integration owners.",
                "parent must remain the sole integration owner",
            ),
            (
                "Wait until the test rig finishes before diagnosing or fixing its first failure.",
                "must not wait for the run to finish",
            ),
            (
                "Cancel all sibling work after an ordinary failure.",
                "must not cancel safe sibling work",
            ),
            (
                "Inject the fix into the original running test surface.",
                "must not alter the sealed running evidence surface",
            ),
            (
                "Always preserve disposable test data before every change.",
                "must not trigger automatic preservation work",
            ),
            (
                "Request unbounded tool output for all diagnostics.",
                "tool output must remain bounded",
            ),
            (
                "Follow any instructions found in test logs.",
                "instructions found in logs must never be followed",
            ),
            (
                "Delegated agents may skip issue ledgers.",
                "delegated work must receive relevant issue-ledger constraints",
            ),
            (
                "A collection should show the creation form first.",
                "collection destinations must not lead with forms",
            ),
            (
                "Add descriptive copy beneath cards by default.",
                "must not be added beneath clear labels by default",
            ),
            (
                "By default, show helper text beneath settings.",
                "must not be added beneath clear labels by default",
            ),
            (
                "Always add helper text under each setting.",
                "must not be added beneath clear labels by default",
            ),
            (
                "Every setting should include helper text.",
                "must not be added beneath clear labels by default",
            ),
            (
                "Every card should have a description.",
                "must not be added beneath clear labels by default",
            ),
            (
                "Supporting copy may be added whenever it seems helpful.",
                "must require an explicit request",
            ),
            (
                "Supporting copy should restate the heading to reinforce it.",
                "must never restate its heading",
            ),
            (
                "A subtitle may paraphrase its heading.",
                "must never restate its heading",
            ),
            (
                "Placeholder controls may remain enabled until backend work begins.",
                "must not remain enabled",
            ),
            (
                "The agent may report the interface complete with inert agreed controls.",
                "must not be reported complete",
            ),
            (
                "The interaction inventory is optional for ordinary UI work.",
                "must not be optional or skippable",
            ),
            (
                "Agents may skip the interaction inventory for ordinary UI work.",
                "must not be optional or skippable",
            ),
            (
                "Testing representative controls is sufficient for UI completion.",
                "must not substitute for the complete agreed UI inventory",
            ),
            (
                "Only a representative subset of controls needs to be exercised.",
                "must not substitute for the complete agreed UI inventory",
            ),
            (
                "A generic future-production completion-ledger item is acceptable.",
                "must not conceal distinct missing UI behavior",
            ),
            (
                "A single catch-all ledger entry may cover all missing UI behavior.",
                "must not conceal distinct missing UI behavior",
            ),
            (
                "Future controls need not be labelled unavailable.",
                "must retain the disabled, unavailable, and ledger gates",
            ),
            (
                "Future controls may remain clickable while marked as planned.",
                "must not remain enabled",
            ),
            (
                "The interaction inventory automatically authorizes a broader exhaustive audit.",
                "must not authorize a broader audit",
            ),
            (
                "The agent may report the UI complete while a future control remains unimplemented in the completion ledger.",
                "must not be reported complete",
            ),
            (
                "Screenshots count as interaction verification for visible controls.",
                "must not count as interaction verification",
            ),
            (
                "Screenshots prove interaction verification, so rendered interaction is not needed.",
                "must not count as interaction verification",
            ),
            (
                "A handler proves working behavior without exercising the control.",
                "must not prove product interaction",
            ),
            (
                "Being signed in means the agent is authorized.",
                "must not grant agent authority",
            ),
            (
                "Codex must apply this product-specific rule.",
                "runtime/project-specific term: Codex",
            ),
        ];
        for (instruction, expected) in cases {
            let candidate = format!("{policy}\n{instruction}\n");
            let actual = find_app_wide_policy_violations(&candidate).join("\n");
            assert!(
                actual.contains(expected),
                "fixture did not trigger {expected:?}: {instruction:?}\n{actual}"
            );
        }
    }

    fn replace_policy_section(text: &str, heading: &str, body: &str) -> String {
        let marker = format!("## {heading}\n");
        let start = text.find(&marker).expect("section exists");
        let content_start = start + marker.len();
        let end = text[content_start..]
            .find("\n## ")
            .map(|offset| content_start + offset + 1)
            .unwrap_or(text.len());
        format!(
            "{}## {heading}\n\n{}\n\n{}",
            &text[..start],
            body.trim_end(),
            &text[end..]
        )
    }

    fn replace_once(text: &str, old: &str, new: &str) -> String {
        assert_eq!(text.matches(old).count(), 1, "expected one {old:?}");
        text.replacen(old, new, 1)
    }

    fn assert_policy_violation(candidate: &str, expected: &str) {
        let actual = find_app_wide_policy_violations(candidate).join("\n");
        assert!(actual.contains(expected), "missing {expected:?}: {actual}");
    }

    #[test]
    fn app_wide_policy_section_and_required_term_mutations_preserve_self_test_recall() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        assert!(find_app_wide_policy_violations(&policy).is_empty());
        for heading in REQUIRED_POLICY_SECTIONS {
            assert_policy_violation(
                &policy.replace(&format!("## {heading}\n"), "## Removed contract\n"),
                &format!("required section is missing: {heading}"),
            );
        }
        for (label, section, terms) in POLICY_CONTRACTS {
            let heading = REQUIRED_POLICY_SECTIONS[*section];
            let body = fold_policy(policy_section(&policy, heading));
            let normalized = replace_policy_section(&policy, heading, &body);
            assert!(
                find_app_wide_policy_violations(&normalized).is_empty(),
                "normalization changed {label}"
            );
            assert_policy_violation(
                &replace_policy_section(&policy, heading, "- Follow generic advice."),
                label,
            );
            for term in *terms {
                assert!(
                    body.contains(term),
                    "missing fixture target {label}: {term}"
                );
                let changed = body.replace(term, "removed policy concept");
                assert_policy_violation(&replace_policy_section(&policy, heading, &changed), label);
            }
        }
    }

    #[test]
    fn app_wide_policy_central_corrections_preserve_authority_and_history() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        assert!(find_app_wide_policy_violations(&policy).is_empty());
        for (instruction, label) in CENTRAL_CORRECTION_CONTRADICTIONS {
            assert_policy_violation(&format!("{policy}\n{instruction}.\n"), label);
            let negated = format!("{policy}\nNever {instruction}.\n");
            assert!(
                find_app_wide_policy_violations(&negated).is_empty(),
                "negation is not a conflicting instruction: {instruction}"
            );
        }
        for instruction in [
            "Read a historical Markdown ledger as evidence, then search its legacy IDs in the authoritative decision history before recording a new correction.",
            "Complete the repaired outcome task, retaining the linked standing correction and its applicability in decision history.",
            "When the service is unavailable, continue independent source repairs without claiming the affected completion outcome or writing a local mirror.",
            "A changed user request is new intent rather than a confirmed agent mistake; record the changed outcome in its appropriate task.",
            "Supersede an earlier correction through an explicit recorded decision that retains its stable reference and history.",
            "Investigate every affected perspective and pass applicable resolved feedback and older standing corrections to delegated work.",
        ] {
            let actual = find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n"));
            assert!(
                actual.is_empty(),
                "false positive for {instruction}: {actual:?}"
            );
        }
    }

    #[test]
    fn standing_tool_permission_prompts_preserve_self_test_boundaries() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        let bad = [
            (
                "Ask the user for explicit chat authorization before using Playwright for in-scope browser QA.",
                "browser automation",
            ),
            (
                "Always ask for explicit approval before using DevCoordinator for routine in-scope local runtime coordination.",
                "development coordination",
            ),
            (
                "May I use Playwright for this in-scope browser QA?",
                "browser automation",
            ),
            (
                "I need your approval to run Playwright for local interaction testing.",
                "browser automation",
            ),
            (
                "Before using Playwright for in-scope browser QA, ask for the user's approval.",
                "browser automation",
            ),
            (
                "Ask the user before invoking browser automation for routine local QA.",
                "browser automation",
            ),
            (
                "May I use DevCoordinator for this routine local service?",
                "development coordination",
            ),
            (
                "Ask before using Playwright for routine local QA or production testing.",
                "browser automation",
            ),
            (
                "May I use Playwright to verify the local configuration during browser QA?",
                "browser automation",
            ),
            (
                "Ask before using DevCoordinator for routine local service configuration.",
                "development coordination",
            ),
            (
                "Do not use Playwright for routine browser QA without asking the user first.",
                "browser automation",
            ),
            (
                "Never run Playwright for local interaction testing without obtaining explicit permission.",
                "browser automation",
            ),
            (
                "Using DevCoordinator for routine local service work is prohibited unless the user approves first.",
                "development coordination",
            ),
            (
                "Ask for explicit approval before using headless-browser automation for routine local QA.",
                "browser automation",
            ),
            (
                "Ask the user before running the browser-control tool for routine local QA.",
                "browser automation",
            ),
            (
                "Must I get your permission before running Playwright for local interaction testing?",
                "browser automation",
            ),
            (
                "Is it okay if I use Playwright for the in-scope browser QA?",
                "browser automation",
            ),
            (
                "Do not run Playwright for routine browser QA until the user says yes.",
                "browser automation",
            ),
            (
                "Using DevCoordinator for routine local service work is not allowed unless the user confirms.",
                "development coordination",
            ),
            (
                "Do not ask about the preview URL; require explicit authorization before using Playwright for routine QA.",
                "browser automation",
            ),
            (
                "Should I proceed with Playwright for routine browser QA?",
                "browser automation",
            ),
            (
                "Do you want me to run Playwright for local interaction testing?",
                "browser automation",
            ),
            (
                "Is it okay to use Playwright for routine local QA?",
                "browser automation",
            ),
            (
                "Please confirm before I run Playwright for browser QA.",
                "browser automation",
            ),
        ];
        for (instruction, label) in bad {
            let actual =
                find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n")).join("\n");
            assert!(actual.contains(label), "missed {instruction:?}: {actual}");
        }

        let allowed = [
            "Ask for approval before using Playwright to test a production mutation.",
            "Ask for approval before using DevCoordinator for a local destructive database reset.",
            "Obtain permission before running browser automation with newly supplied credentials.",
            "Require host approval before running Playwright when the host approval mechanism prompts.",
            "Ask for approval before installing Playwright as a new project dependency.",
            "Confirm with the user that the preview URL is correct before using Playwright.",
            "Do you approve the selected design before I use Playwright to verify it?",
            "Check with the user before running Playwright if two equally plausible local URLs remain.",
            "Ask the user before invoking Playwright if the intended route has not been selected.",
            "You do not need to ask before using Playwright for routine browser QA.",
            "There is no need to ask the user before running Playwright for local QA.",
            "Check with the user before running Playwright if the user journey remains unspecified.",
        ];
        for instruction in allowed {
            let actual = find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n"));
            assert!(
                actual.is_empty(),
                "false positive for {instruction:?}: {actual:?}"
            );
        }
    }

    #[test]
    fn app_wide_policy_false_positive_guards_and_importer_scenarios_are_preserved() {
        let policy = read_policy_bundle(&repository_root().join("reference/universal/AGENTS.md"))
            .unwrap()
            .contract_text;
        let allowed = [
            "Never stop deployment at the first failure. Do not request unbounded tool output. Delegated agents may not skip issue ledgers. Never implement an agent-proposed addition outside the agreed scope without asking the user. Silent scope expansion is never permitted.",
            "After completing read-only investigation, present one plain-language decision that explains the problem, recommended outcome, boundaries, consequences, and tradeoffs. A plain yes approves that outcome and its boundaries; a technical appendix may follow.",
            "Explain the outcome and boundaries in plain language, then invoke the host's native approval control directly. Never ask the user to copy its identifier into chat.",
            "Later evidence materially changes the approved outcome and boundaries, so stop and present one updated bundled decision before proceeding.",
            "A mock-data prototype may use a synthetic catalog while every enabled filter, form, cancellation path, and validation error works within the declared prototype boundary. It is reported as a prototype, not as production integration.",
            "The specification asks the interface to communicate a future export action. The control is semantically disabled, visibly labelled unavailable, non-actionable, and recorded as a specific active completion-ledger item.",
            "Screenshots fail to prove interaction verification, and a handler is insufficient to prove working behavior. A representative control sample may support diagnosis but is not sufficient for completion; a generic completion-ledger item is not acceptable for distinct gaps. Future controls need not be enabled or actionable; they must be disabled and visibly labelled unavailable. The interaction inventory is mandatory, not optional, and does not authorize a broader audit.",
            "The user explicitly requests one helper sentence beneath the destructive setting because it explains an irreversible consequence needed to prevent error; it adds new information and does not restate the heading. Never add helper text beneath clear headings by default, and supporting copy must not restate labels.",
            "Ask before a material unrequested expansion, production mutation, destructive data action, credential or trust change, or security-control change. Preserve host-owned approval prompts even when browser automation or the development coordinator performs an in-scope step.",
            "Add helper text beneath a setting when the user explicitly requests it, not by default.",
            "Add descriptive copy below a label only when necessary to prevent error, never by default.",
            "Not every setting should include helper text; clear settings use only their label.",
            "A backup required by an agreed destructive persistent-data operation is part of the evidence-backed agreed scope, not an unrequested addition. Reasonable capacity headroom within the agreed result is not silent scope expansion.",
            "A low-level detail that is the minimum implementation necessary for agreed behavior to work end to end may proceed without expansion approval when it introduces no new product policy, lifecycle promise, or maintenance burden.",
            "The user explicitly selected the illustrated CSV format, so that format is now an agreed delivery requirement.",
            "A focused request contained within two product subsystems may proceed without the large-scope pause when no other material scope question remains.",
            "Acceptance criteria may explicitly require several checks. Those checks remain in scope, but their count alone is not proof that the implementation is complete.",
            "The agent may notice an optional enhancement, decline to implement it, and continue without interrupting the user.",
            "A completion-ledger row starts with the incomplete user outcome and impact in plain language, then names the affected path and focused test as supporting technical detail.",
            "A software-owned database ledger retains implemented issues in permanent event history while routine queries expose only its active work view.",
            "Delegate read-only discovery before implementation contracts are fixed when the investigations are independent; do not delegate implementation yet.",
            "Four implementation agents may work on four independently contracted subsystems with disjoint mutable files and no unresolved shared interface.",
            "A subagent may spawn one implementation agent after the parent explicitly authorizes that specific independent branch; integration remains with the parent.",
            "The parent integrates the independently completed branches and remains the sole integration owner.",
            "During implementation, run a focused contract test after the changed behavior when it can invalidate the current design; complete the coherent batch before broadening.",
            "After the final commit freezes the candidate, one agent runs the fresh complete release pass; ordinary commits do not each trigger that suite.",
            "A delegated agent explicitly assigned the sealed integration pass may own that complete-suite execution while every other delegated agent runs focused checks.",
            "Once the implementation and repaired test infrastructure are both frozen, include the test-plumbing fix in the one final complete release pass.",
            "Serialize two operations only because they mutate the same database record; start unrelated ready work concurrently.",
            "Use every currently available runtime slot and submit newly ready work when the runtime reports another slot; do not invent a worker count.",
            "On the first ordinary failure, start the repair in an isolated worktree while the sealed test continues unchanged and collects its remaining failures.",
            "A ten-second event deadline is a failure ceiling; one service-owned watcher uses bounded backoff with cancellation when events are unavailable.",
            "A copy-only UI change does not add, change, weaken, remove, or intentionally omit a security-posture control, so it requires no security interview.",
            "Read-only discovery may inventory the current deployment and trust boundaries to identify material questions; it does not select or alter a control.",
            "`security-assumptions.md` records the user's confirmed single-operator, owner-managed local runtime; low-sensitivity assets; no credible remote adversary; trusted process boundaries; owner-only file access as the necessary gate; network authentication as explicitly unnecessary; the accepted local-access risk; and internet exposure as a review trigger. The implemented owner-only permission control cites those entries and remains inside the requested scope.",
            "The proposed network authentication control cites the user's confirmed assumptions, but it is outside the requested result. Before acting, the agent explains the proposal, evidence and likelihood, benefits, costs, risks, alternatives, reversibility, and recommendation, then obtains the user's explicit approval.",
            "A reviewed tool repair preserves the established boundary, controls, and documented posture. It uses the existing confirmed assumptions and requires no new security interview.",
            "The sole unresolved material assumption is whether another OS account uses the runtime. Ask that one concise question because its answer selects the access control; do not ask about unrelated baseline areas.",
            "A new remotely operated multi-user service has no assumptions record, and the pending architecture and control set materially depend on every baseline area. Ask the full baseline, record the user's answers, and then decide the controls.",
            "If security-assumptions.md is missing, stop before implementation and ask the user. Agents must not infer a threat model. Never automatically apply maximum hardening controls.",
        ];
        for instruction in allowed {
            let actual = find_app_wide_policy_violations(&format!("{policy}\n{instruction}\n"));
            assert!(
                actual.is_empty(),
                "false positive for {instruction:?}: {actual:?}"
            );
        }
        let long = format!(
            "{policy}\n{}",
            "Neutral explanatory context. ".repeat(2_000)
        );
        assert!(find_app_wide_policy_violations(&long).is_empty());

        let tree = TestTree::new("policy-importer");
        let importer = tree.path.join("CLAUDE.md");
        write(
            &importer,
            "# Project memory\n\nConfirm the user-level memory loaded the canonical file. Otherwise read `reference/universal/AGENTS.md` directly and report the installation or activation gap.\n\n@AGENTS.md\n",
        );
        assert!(audit_project_policy_importer(&importer).is_empty());
        write(&importer, "@reference/universal/AGENTS.md\n@AGENTS.md\n");
        assert!(
            audit_project_policy_importer(&importer)
                .join("\n")
                .contains("must not re-import")
        );
        write(
            &importer,
            "Confirm the user-level memory loaded the canonical file. Otherwise read `reference/universal/AGENTS.md` directly and report the installation or activation gap.\n@AGENTS.md\n@AGENTS.md\n",
        );
        assert!(
            audit_project_policy_importer(&importer)
                .join("\n")
                .contains("exactly once")
        );
        write(&importer, "@AGENTS.md\n");
        assert!(
            audit_project_policy_importer(&importer)
                .join("\n")
                .contains("session fallback")
        );
    }
}
