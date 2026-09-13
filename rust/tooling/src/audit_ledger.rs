//! Strict parser for retired `CompletionLedger.md` snapshots.
//!
//! The planning database is the live authority. This module exists only for
//! migration and audit fixtures that still contain the historical Markdown
//! format. Reads are descriptor-relative and refuse symlinks so a repository
//! cannot redirect a privileged audit to another file.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use rustix::fs::{self as unix_fs, AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};

pub const TITLE: &str = "# Completion Ledger";
pub const HEADERS: [&str; 5] = [
    "ID",
    "Remaining work",
    "Why it matters",
    "Status",
    "Verification",
];

const ACTIVE_STATUSES: [&str; 11] = [
    "active",
    "blocked",
    "in progress",
    "incomplete",
    "open",
    "partial",
    "pending",
    "to do",
    "todo",
    "unresolved",
    "waiting",
];
const TERMINAL_STATUSES: [&str; 9] = [
    "closed",
    "complete",
    "completed",
    "done",
    "fixed",
    "implemented",
    "implemented and verified",
    "resolved",
    "verified",
];
const FUTURE_STATUS_MARKERS: [&str; 6] = ["after", "before", "once", "unless", "until", "when"];
const NEGATION_QUALIFIERS: [&str; 7] = [
    "actually",
    "completely",
    "currently",
    "fully",
    "properly",
    "successfully",
    "yet",
];
const FUTURE_STATUS_PHRASES: [&[&str]; 5] = [
    &["must", "be"],
    &["need", "to", "be"],
    &["needs", "to", "be"],
    &["to", "be"],
    &["will", "be"],
];

static CONTRACTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:isn|wasn|hasn|hadn|doesn|didn|won|can|couldn|shouldn|wouldn)['’]t\b")
        .expect("constant contraction regex")
});
static SPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\s_-]+").expect("constant space regex"));
static REQUIRED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:is|are|be|must be|needs? to be)\s+required\b")
        .expect("constant required regex")
});
static CLAUSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:[/|,;]|\s[-–—:]\s)").expect("constant clause regex"));
static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z0-9]+").expect("constant word regex"));
static SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^:?-{3,}:?$").expect("constant separator regex"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerError(pub String);

impl fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LedgerError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LedgerRow {
    pub id: String,
    pub remaining_work: String,
    pub why_it_matters: String,
    pub status: String,
    pub verification: String,
}

impl LedgerRow {
    fn values(&self) -> [(&'static str, &str); 5] {
        [
            ("id", &self.id),
            ("remaining_work", &self.remaining_work),
            ("why_it_matters", &self.why_it_matters),
            ("status", &self.status),
            ("verification", &self.verification),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusClassification {
    Active,
    Terminal,
}

fn status_key(value: &str) -> String {
    let unstyled = value
        .chars()
        .filter(|character| !matches!(character, '`' | '*' | '_' | '~'))
        .collect::<String>()
        .trim()
        .to_lowercase();
    let expanded = CONTRACTION_RE.replace_all(&unstyled, " not ");
    SPACE_RE
        .replace_all(&expanded, " ")
        .trim_matches(|character: char| " .:;()[]{}".contains(character))
        .to_owned()
}

fn prefix_end(value: &str, choice: &str) -> Option<usize> {
    if value == choice {
        return Some(choice.len());
    }
    let rest = value.strip_prefix(choice)?;
    let next = rest.chars().next()?;
    (!next.is_alphanumeric() && next != '_').then_some(choice.len())
}

fn matching_prefix<'a>(value: &str, choices: &'a [&str]) -> Option<(&'a str, usize)> {
    let mut ordered = choices.to_vec();
    ordered.sort_by_key(|choice| std::cmp::Reverse(choice.len()));
    ordered
        .into_iter()
        .find_map(|choice| prefix_end(value, choice).map(|end| (choice, end)))
}

fn terminal_match_is_pending(key: &str, active_end: usize, terminal_start: usize) -> bool {
    let context = &key[active_end..terminal_start];
    let trailing = &key[terminal_start..];
    if REQUIRED_RE.is_match(trailing) {
        return true;
    }
    let clause = CLAUSE_RE.split(context).last().unwrap_or(context);
    let words = WORD_RE
        .find_iter(clause)
        .map(|capture| capture.as_str())
        .collect::<Vec<_>>();
    if words
        .iter()
        .any(|word| FUTURE_STATUS_MARKERS.contains(word))
    {
        return true;
    }
    for index in (0..words.len()).rev() {
        match words[index] {
            "not" | "never"
                if words[index + 1..]
                    .iter()
                    .all(|word| NEGATION_QUALIFIERS.contains(word)) =>
            {
                return true;
            }
            "not" | "never" => break,
            "no" if matches!(words[index + 1..], ["longer"] | ["longer", "fully"]) => {
                return true;
            }
            "no" => break,
            "without" if matches!(words[index + 1..], ["being"] | ["being", "fully"]) => {
                return true;
            }
            "without" => break,
            _ => {}
        }
    }
    FUTURE_STATUS_PHRASES
        .iter()
        .any(|phrase| words.ends_with(phrase))
}

pub fn status_classification(value: &str) -> Option<StatusClassification> {
    let key = status_key(value);
    let active_prefix = matching_prefix(&key, &ACTIVE_STATUSES);
    if matching_prefix(&key, &TERMINAL_STATUSES).is_some() {
        return Some(StatusClassification::Terminal);
    }
    let (_, active_end) = active_prefix?;
    let mut terminal_choices = TERMINAL_STATUSES;
    terminal_choices.sort_by_key(|choice| std::cmp::Reverse(choice.len()));
    for status in terminal_choices {
        let expression = Regex::new(&format!(r"\b{}(?:\b|$)", regex::escape(status)))
            .expect("escaped status regex");
        for terminal in expression.find_iter(&key) {
            if terminal.start() < active_end {
                continue;
            }
            if terminal_match_is_pending(&key, active_end, terminal.start()) {
                continue;
            }
            return Some(StatusClassification::Terminal);
        }
    }
    Some(StatusClassification::Active)
}

fn blocked_condition_is_meaningful(value: &str) -> bool {
    let key = status_key(value);
    let Some((_, end)) = matching_prefix(&key, &["blocked"]) else {
        return true;
    };
    WORD_RE.find_iter(&key[end..]).count() >= 2
}

fn split_row(line: &str) -> Result<Vec<String>, LedgerError> {
    let stripped = line.trim();
    if !stripped.starts_with('|') || !stripped.ends_with('|') {
        return Err(LedgerError(format!(
            "ledger table row must begin and end with '|': {line:?}"
        )));
    }
    let body = &stripped[1..stripped.len() - 1];
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut characters = body.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' && matches!(characters.peek(), Some('\\' | '|')) {
            current.push(characters.next().expect("peeked character"));
        } else if character == '|' {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(character);
        }
    }
    cells.push(current.trim().to_owned());
    Ok(cells)
}

fn is_separator(cells: &[String]) -> bool {
    cells.len() == HEADERS.len() && cells.iter().all(|cell| SEPARATOR_RE.is_match(cell))
}

pub fn validate_rows(rows: &[LedgerRow], allow_empty: bool) -> Result<(), LedgerError> {
    if rows.is_empty() && !allow_empty {
        return Err(LedgerError(
            "present CompletionLedger.md must contain at least one active row".to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    for row in rows {
        let values = row.values();
        let empty = values
            .iter()
            .filter_map(|(name, value)| value.trim().is_empty().then_some(*name))
            .collect::<Vec<_>>();
        if !empty.is_empty() {
            return Err(LedgerError(format!(
                "ledger row {:?} has empty fields: {}",
                row.id,
                empty.join(", ")
            )));
        }
        let multiline = values
            .iter()
            .filter_map(|(name, value)| value.contains(['\n', '\r']).then_some(*name))
            .collect::<Vec<_>>();
        if !multiline.is_empty() {
            return Err(LedgerError(format!(
                "ledger row {:?} has multiline fields: {}",
                row.id,
                multiline.join(", ")
            )));
        }
        let padded = values
            .iter()
            .filter_map(|(name, value)| (*value != value.trim()).then_some(*name))
            .collect::<Vec<_>>();
        if !padded.is_empty() {
            return Err(LedgerError(format!(
                "ledger row {:?} has leading or trailing whitespace: {}",
                row.id,
                padded.join(", ")
            )));
        }
        let folded = row.id.to_lowercase();
        if !seen.insert(folded) {
            return Err(LedgerError(format!(
                "duplicate completion-ledger ID: {}",
                row.id
            )));
        }
        match status_classification(&row.status) {
            Some(StatusClassification::Terminal) => {
                return Err(LedgerError(format!(
                    "terminal Status {:?} must be removed for row {}",
                    row.status, row.id
                )));
            }
            Some(StatusClassification::Active) => {}
            None => {
                return Err(LedgerError(format!(
                    "unrecognized Status {:?} for row {}; use an active status",
                    row.status, row.id
                )));
            }
        }
        if !blocked_condition_is_meaningful(&row.status) {
            return Err(LedgerError(format!(
                "blocked Status for row {} must name a meaningful unblock condition",
                row.id
            )));
        }
    }
    Ok(())
}

pub fn parse_ledger(text: &str) -> Result<Vec<LedgerRow>, LedgerError> {
    let nonblank = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if nonblank.len() < 4 || nonblank[0] != TITLE {
        return Err(LedgerError(format!(
            "ledger must contain only {TITLE:?} and one active table"
        )));
    }
    let header = split_row(nonblank[1])?;
    if header != HEADERS {
        return Err(LedgerError(format!(
            "completion-ledger columns must be exactly: {}",
            HEADERS.join(" | ")
        )));
    }
    if !is_separator(&split_row(nonblank[2])?) {
        return Err(LedgerError(
            "completion-ledger table separator is malformed".to_owned(),
        ));
    }
    let mut rows = Vec::new();
    for line in &nonblank[3..] {
        let cells = split_row(line)?;
        if cells.len() != HEADERS.len() {
            return Err(LedgerError(format!(
                "completion-ledger row has {} cells; expected {}",
                cells.len(),
                HEADERS.len()
            )));
        }
        let mut cells = cells.into_iter();
        rows.push(LedgerRow {
            id: cells.next().expect("validated cell count"),
            remaining_work: cells.next().expect("validated cell count"),
            why_it_matters: cells.next().expect("validated cell count"),
            status: cells.next().expect("validated cell count"),
            verification: cells.next().expect("validated cell count"),
        });
    }
    validate_rows(&rows, false)?;
    Ok(rows)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u64,
    changed_seconds: i64,
    changed_nanoseconds: u64,
}

impl FileIdentity {
    fn from_stat(stat: &unix_fs::Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            size: stat.st_size as u64,
            modified_seconds: stat.st_mtime,
            modified_nanoseconds: stat.st_mtime_nsec,
            changed_seconds: stat.st_ctime,
            changed_nanoseconds: stat.st_ctime_nsec,
        }
    }
}

fn absolute_components(path: &Path) -> Result<(PathBuf, Vec<OsString>), LedgerError> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(LedgerError(format!(
            "ledger path contains a parent traversal component: {}",
            path.display()
        )));
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| LedgerError(format!("cannot resolve ledger path: {error}")))?
            .join(path)
    };
    let components = absolute
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            Component::CurDir | Component::RootDir => None,
            Component::ParentDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>();
    let mut normalized = PathBuf::from("/");
    normalized.extend(&components);
    Ok((normalized, components))
}

fn directory_traversal_flags() -> OFlags {
    // Known child access needs search permission, not directory enumeration.
    // Keep NOFOLLOW and DIRECTORY so traversal cannot follow a replacement link.
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(not(target_os = "linux"))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

fn open_directory_path(
    components: &[OsString],
    missing_is_absent: bool,
) -> Result<Option<File>, LedgerError> {
    let mut directory = unix_fs::open("/", directory_traversal_flags(), Mode::empty())
        .map(File::from)
        .map_err(|error| LedgerError(format!("ledger anchor is unsafe: {error}")))?;
    for component in components {
        match unix_fs::openat(
            &directory,
            component,
            directory_traversal_flags(),
            Mode::empty(),
        ) {
            Ok(next) => directory = File::from(next),
            Err(rustix::io::Errno::NOENT) if missing_is_absent => return Ok(None),
            Err(error) => {
                return Err(LedgerError(format!(
                    "ledger path contains a symlink or unsafe directory component: {error}"
                )));
            }
        }
    }
    Ok(Some(directory))
}

pub fn validate_directory_nofollow(path: &Path) -> Result<PathBuf, LedgerError> {
    let (absolute, components) = absolute_components(path)?;
    let directory = open_directory_path(&components, false)?
        .ok_or_else(|| LedgerError(format!("directory does not exist: {}", path.display())))?;
    let metadata = unix_fs::fstat(&directory)
        .map_err(|error| LedgerError(format!("cannot inspect directory: {error}")))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
        return Err(LedgerError(format!(
            "path is not a non-symlinked directory: {}",
            path.display()
        )));
    }
    Ok(absolute)
}

pub fn create_directory_all_nofollow(path: &Path, mode: u32) -> Result<PathBuf, LedgerError> {
    let (absolute, components) = absolute_components(path)?;
    let mut directory = unix_fs::open("/", directory_traversal_flags(), Mode::empty())
        .map(File::from)
        .map_err(|error| LedgerError(format!("directory root is unsafe: {error}")))?;
    for component in components {
        let next = match unix_fs::openat(
            &directory,
            &component,
            directory_traversal_flags(),
            Mode::empty(),
        ) {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) => {
                unix_fs::mkdirat(&directory, &component, Mode::from_raw_mode(mode)).map_err(
                    |error| {
                        LedgerError(format!(
                            "cannot create non-symlinked directory {}: {error}",
                            path.display()
                        ))
                    },
                )?;
                unix_fs::openat(
                    &directory,
                    &component,
                    directory_traversal_flags(),
                    Mode::empty(),
                )
                .map_err(|error| {
                    LedgerError(format!(
                        "cannot open created directory {}: {error}",
                        path.display()
                    ))
                })?
            }
            Err(error) => {
                return Err(LedgerError(format!(
                    "directory path contains a symlink or unsafe component: {}: {error}",
                    path.display()
                )));
            }
        };
        directory = File::from(next);
    }
    Ok(absolute)
}

pub fn write_bytes_nofollow(path: &Path, bytes: &[u8], mode: u32) -> Result<(), LedgerError> {
    let (_, components) = absolute_components(path)?;
    if components.is_empty() {
        return Err(LedgerError(format!(
            "output path is not a regular file: {}",
            path.display()
        )));
    }
    let (parent_components, name) = components.split_at(components.len() - 1);
    let parent = open_directory_path(parent_components, false)?
        .ok_or_else(|| LedgerError(format!("output parent does not exist: {}", path.display())))?;
    let descriptor = unix_fs::openat(
        &parent,
        &name[0],
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::TRUNC
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::from_raw_mode(mode),
    )
    .map_err(|error| {
        LedgerError(format!(
            "output path is symlinked or unsafe: {}: {error}",
            path.display()
        ))
    })?;
    let mut file = File::from(descriptor);
    let opened = unix_fs::fstat(&file)
        .map_err(|error| LedgerError(format!("cannot inspect output: {error}")))?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile {
        return Err(LedgerError(format!(
            "output path is not a regular file: {}",
            path.display()
        )));
    }
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| LedgerError(format!("cannot publish {}: {error}", path.display())))?;
    let written = unix_fs::fstat(&file)
        .map_err(|error| LedgerError(format!("cannot inspect published output: {error}")))?;
    let path_after =
        unix_fs::statat(&parent, &name[0], AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
            LedgerError(format!(
                "output was replaced while it was written: {}: {error}",
                path.display()
            ))
        })?;
    if FileType::from_raw_mode(path_after.st_mode) != FileType::RegularFile
        || (written.st_dev, written.st_ino) != (path_after.st_dev, path_after.st_ino)
    {
        return Err(LedgerError(format!(
            "output was replaced while it was written: {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn write_new_bytes_nofollow(path: &Path, bytes: &[u8], mode: u32) -> Result<(), LedgerError> {
    let (_, components) = absolute_components(path)?;
    if components.is_empty() {
        return Err(LedgerError(format!(
            "output path is not a regular file: {}",
            path.display()
        )));
    }
    let (parent_components, name) = components.split_at(components.len() - 1);
    let parent = open_directory_path(parent_components, false)?
        .ok_or_else(|| LedgerError(format!("output parent does not exist: {}", path.display())))?;
    let descriptor = unix_fs::openat(
        &parent,
        &name[0],
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::from_raw_mode(mode),
    )
    .map_err(|error| {
        LedgerError(format!(
            "output must be a new non-symlink file: {}: {error}",
            path.display()
        ))
    })?;
    let mut file = File::from(descriptor);
    let metadata = unix_fs::fstat(&file)
        .map_err(|error| LedgerError(format!("cannot inspect new output: {error}")))?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
        return Err(LedgerError(format!(
            "new output is not a regular file: {}",
            path.display()
        )));
    }
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| LedgerError(format!("cannot publish {}: {error}", path.display())))
}

/// Read a regular file without following any supplied path symlink.
///
/// `Ok(None)` means the path does not exist. Any other unsafe object or
/// unstable read fails closed.
pub fn read_bytes_nofollow(
    path: &Path,
    anchor: Option<&Path>,
) -> Result<Option<Vec<u8>>, LedgerError> {
    let (absolute, components) = absolute_components(path)?;
    if components.is_empty() {
        return Err(LedgerError(format!(
            "ledger path is not a regular file: {}",
            path.display()
        )));
    }
    if let Some(anchor) = anchor {
        let (anchor_absolute, _) = absolute_components(anchor).map_err(|_| {
            LedgerError(format!(
                "ledger anchor contains a parent traversal component: {}",
                anchor.display()
            ))
        })?;
        if !absolute.starts_with(&anchor_absolute) {
            return Err(LedgerError(format!(
                "ledger path escapes its trusted project root: {}",
                path.display()
            )));
        }
    }
    let (parent_components, name) = components.split_at(components.len() - 1);
    let Some(parent) = open_directory_path(parent_components, true)? else {
        return Ok(None);
    };
    let name = &name[0];
    let descriptor = match unix_fs::openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(LedgerError(format!(
                "ledger path is symlinked or unsafe: {}: {error}",
                path.display()
            )));
        }
    };
    let mut file = File::from(descriptor);
    let before = unix_fs::fstat(&file)
        .map_err(|error| LedgerError(format!("could not inspect ledger: {error}")))?;
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
        return Err(LedgerError(format!(
            "ledger path is not a regular file: {}",
            path.display()
        )));
    }
    let before_identity = FileIdentity::from_stat(&before);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| LedgerError(format!("could not read ledger: {error}")))?;
    let after = unix_fs::fstat(&file)
        .map_err(|error| LedgerError(format!("could not inspect ledger after read: {error}")))?;
    if FileIdentity::from_stat(&after) != before_identity {
        return Err(LedgerError(format!(
            "ledger changed while it was being read: {}",
            path.display()
        )));
    }
    let path_after =
        unix_fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
            LedgerError(format!(
                "ledger was replaced while it was being read: {}: {error}",
                path.display()
            ))
        })?;
    if FileType::from_raw_mode(path_after.st_mode) != FileType::RegularFile
        || FileIdentity::from_stat(&path_after) != before_identity
    {
        return Err(LedgerError(format!(
            "ledger was replaced while it was being read: {}",
            path.display()
        )));
    }
    let rebound = open_directory_path(parent_components, false)?.expect("missing disallowed");
    let original_parent = unix_fs::fstat(&parent)
        .map_err(|error| LedgerError(format!("cannot inspect ledger parent: {error}")))?;
    let rebound_parent = unix_fs::fstat(&rebound)
        .map_err(|error| LedgerError(format!("cannot inspect rebound ledger parent: {error}")))?;
    if (original_parent.st_dev, original_parent.st_ino)
        != (rebound_parent.st_dev, rebound_parent.st_ino)
    {
        return Err(LedgerError(format!(
            "ledger parent path changed while it was being read: {}",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

pub fn read_text_nofollow(
    path: &Path,
    anchor: Option<&Path>,
) -> Result<Option<String>, LedgerError> {
    let Some(bytes) = read_bytes_nofollow(path, anchor)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| LedgerError(format!("ledger is not valid UTF-8: {}", path.display())))
}

fn escape_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

pub fn render_row(row: &LedgerRow) -> Result<String, LedgerError> {
    validate_rows(std::slice::from_ref(row), false)?;
    Ok(format!(
        "| {} |",
        row.values()
            .iter()
            .map(|(_, value)| escape_cell(value))
            .collect::<Vec<_>>()
            .join(" | ")
    ))
}

pub fn render_ledger(rows: &[LedgerRow]) -> Result<String, LedgerError> {
    validate_rows(rows, false)?;
    let mut lines = vec![
        TITLE.to_owned(),
        String::new(),
        format!("| {} |", HEADERS.join(" | ")),
        format!("| {} |", ["---"; 5].join(" | ")),
    ];
    for row in rows {
        lines.push(render_row(row)?);
    }
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str) -> LedgerRow {
        LedgerRow {
            id: "A-1".to_owned(),
            remaining_work: "Finish the adapter".to_owned(),
            why_it_matters: "Users cannot run the audit".to_owned(),
            status: status.to_owned(),
            verification: "Run the Rust self-test".to_owned(),
        }
    }

    #[test]
    fn active_and_terminal_statuses_keep_pending_qualifiers_distinct() {
        assert_eq!(
            status_classification("Open — not implemented"),
            Some(StatusClassification::Active)
        );
        assert_eq!(
            status_classification("Blocked — until rollout is completed"),
            Some(StatusClassification::Active)
        );
        assert_eq!(
            status_classification("Open — implementation completed"),
            Some(StatusClassification::Terminal)
        );
        assert_eq!(
            status_classification("implemented and verified"),
            Some(StatusClassification::Terminal)
        );
        assert_eq!(status_classification("mystery"), None);
    }

    #[test]
    fn parser_round_trips_escaped_cells() {
        let mut value = row("In progress");
        value.remaining_work = r"Keep A | B and C \\ D".to_owned();
        let rendered = render_ledger(std::slice::from_ref(&value)).unwrap();
        assert_eq!(parse_ledger(&rendered).unwrap(), vec![value]);
    }

    #[test]
    fn parser_rejects_terminal_duplicate_and_vague_blocked_rows() {
        assert!(validate_rows(&[row("Done")], false).is_err());
        assert!(validate_rows(&[row("Blocked")], false).is_err());
        let duplicate = row("Open");
        let mut same = duplicate.clone();
        same.id = "a-1".to_owned();
        assert!(validate_rows(&[duplicate, same], false).is_err());
    }

    #[test]
    fn nofollow_reader_rejects_symlinks_and_reads_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = directory.path().join("CompletionLedger.md");
        std::fs::write(&ledger, render_ledger(&[row("Open")]).unwrap()).unwrap();
        assert!(
            read_text_nofollow(&ledger, Some(directory.path()))
                .unwrap()
                .is_some()
        );
        let link = directory.path().join("linked.md");
        std::os::unix::fs::symlink(&ledger, &link).unwrap();
        assert!(read_text_nofollow(&link, Some(directory.path())).is_err());
    }

    #[test]
    fn nofollow_reader_rejects_anchor_escape_and_parent_traversal() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            read_text_nofollow(&directory.path().join("../outside"), Some(directory.path()))
                .is_err()
        );
        assert!(read_text_nofollow(Path::new("/etc/hosts"), Some(directory.path())).is_err());
    }

    #[test]
    fn nofollow_writer_rejects_symlink_and_fifo_targets() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        write_bytes_nofollow(&target, b"{}\n", 0o600).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"{}\n");
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_bytes_nofollow(&link, b"bad", 0o600).is_err());
        let fifo = directory.path().join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        assert!(write_bytes_nofollow(&fifo, b"bad", 0o600).is_err());
    }

    #[test]
    fn nofollow_directory_creation_refuses_parent_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let created = directory.path().join("a/b/c");
        assert_eq!(
            create_directory_all_nofollow(&created, 0o700).unwrap(),
            created
        );
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("linked")).unwrap();
        assert!(
            create_directory_all_nofollow(&directory.path().join("linked/child"), 0o700).is_err()
        );
    }

    #[test]
    fn exclusive_nofollow_writer_never_replaces_existing_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("review.json");
        write_new_bytes_nofollow(&path, b"first", 0o600).unwrap();
        assert!(write_new_bytes_nofollow(&path, b"second", 0o600).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"first");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nofollow_paths_traverse_without_directory_listing_permission() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("permission checks require an unprivileged test process");
            return;
        }
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700))
                    .expect("restore isolated fixture for cleanup");
            }
        }
        let fixture = tempfile::tempdir().unwrap();
        let traversal = fixture.path().join("traversal-only");
        let leaf = traversal.join("owned-evidence");
        std::fs::create_dir_all(&leaf).unwrap();
        let input = leaf.join("report.json");
        std::fs::write(&input, b"verified evidence").unwrap();
        symlink(&leaf, traversal.join("redirected")).unwrap();
        let _restore = Restore(traversal.clone());
        std::fs::set_permissions(&traversal, std::fs::Permissions::from_mode(0o111)).unwrap();
        assert!(std::fs::read_dir(&traversal).is_err());
        assert_eq!(std::fs::read(&input).unwrap(), b"verified evidence");
        assert_eq!(
            read_bytes_nofollow(&input, Some(fixture.path()))
                .unwrap()
                .unwrap(),
            b"verified evidence"
        );
        assert_eq!(validate_directory_nofollow(&leaf).unwrap(), leaf);
        let output = leaf.join("review.json");
        write_new_bytes_nofollow(&output, b"reviewed", 0o600).unwrap();
        assert!(write_new_bytes_nofollow(&output, b"replace", 0o600).is_err());
        write_bytes_nofollow(&output, b"updated", 0o600).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"updated");
        let created = leaf.join("new/nested");
        assert_eq!(
            create_directory_all_nofollow(&created, 0o700).unwrap(),
            created
        );
        assert!(read_bytes_nofollow(&traversal.join("redirected/report.json"), None).is_err());
        assert!(create_directory_all_nofollow(&traversal.join("redirected/new"), 0o700).is_err());
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(read_bytes_nofollow(&input, None).is_err());
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&traversal, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(read_bytes_nofollow(&input, None).is_err());
        assert!(std::fs::read_dir(&traversal).is_err());
    }
}
