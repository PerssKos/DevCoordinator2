//! Daemon-independent open bug registry.
//!
//! The registry deliberately does not depend on the authority database or the
//! daemon socket. Once the configured directory is opened, access is
//! descriptor-relative and refuses symlinks. The directory may intentionally
//! be writable by every trusted local caller.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use devcoordinator2_api::{params, results};
use regex::Regex;
use rustix::fs::{self as unix_fs, AtFlags, Dir, FlockOperation, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

const DEFAULT_DIRECTORY: &str = "/var/lib/devcoordinator2-bugs";
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_COMPONENT_CHARS: usize = 64;
const MAX_SUMMARY_CHARS: usize = 200;
const MAX_EXPECTED_CHARS: usize = 2_000;
const MAX_ACTUAL_CHARS: usize = 2_000;
const MAX_STEPS_CHARS: usize = 4_000;
const MAX_CORRELATION_CHARS: usize = 128;
const MAX_REPORTER_CHARS: usize = 128;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

static FORBIDDEN_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(password|secret|token|api[_-]?key|authorization:\s*bearer)")
        .expect("the bug secret detector is valid")
});
static PRIVATE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/home/[^/\s]+/|/etc/devcoordinator2/|/var/lib/devcoordinator2/")
        .expect("the bug private-path detector is valid")
});

#[derive(Debug, Error)]
pub enum BugError {
    #[error("{0}")]
    Invalid(String),
    #[error("cannot {operation} bug store: {source}")]
    Store {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cannot generate bug id: {0}")]
    Id(#[from] crate::ids::IdError),
    #[error("cannot format bug timestamp: {0}")]
    Time(#[from] time::error::Format),
    #[error("cannot encode bug record: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBugRecord {
    component: String,
    summary: String,
    expected: String,
    actual: String,
    steps: String,
    correlations: results::BugCorrelations,
    bug_id: String,
    opened_at: String,
    last_seen_at: String,
    occurrences: u32,
    reporter: String,
}

/// Resolve the standalone store without loading daemon-private configuration.
pub fn bugs_dir() -> PathBuf {
    std::env::var_os("DEVCOORDINATOR2_BUGS_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIRECTORY))
}

/// Create a bug or count a recurrence with the same component and summary.
pub fn report(
    directory: &Path,
    request: &params::BugReport,
    reporter: &str,
) -> Result<results::BugRecord, BugError> {
    let mut candidate = StoredBugRecord {
        component: clean("component", &request.component, MAX_COMPONENT_CHARS)?,
        summary: clean("summary", &request.summary, MAX_SUMMARY_CHARS)?,
        expected: clean("expected", &request.expected, MAX_EXPECTED_CHARS)?,
        actual: clean("actual", &request.actual, MAX_ACTUAL_CHARS)?,
        steps: clean("steps", &request.steps, MAX_STEPS_CHARS)?,
        correlations: validated_correlations(&request.correlations)?,
        bug_id: String::new(),
        opened_at: String::new(),
        last_seen_at: String::new(),
        occurrences: 1,
        reporter: reporter.chars().take(MAX_REPORTER_CHARS).collect(),
    };

    let directory = open_store(directory, true)?.expect("created store must be present");
    lock(&directory, FlockOperation::LockExclusive, "lock for report")?;
    for mut existing in read_open_records(&directory)? {
        if existing.component == candidate.component && existing.summary == candidate.summary {
            existing.occurrences = existing.occurrences.checked_add(1).ok_or_else(|| {
                BugError::Invalid("bug occurrence count exceeds supported range".into())
            })?;
            existing.last_seen_at = now()?;
            merge_correlations(&mut existing.correlations, &candidate.correlations);
            atomic_write(&directory, &existing)?;
            return Ok(to_result(existing, true));
        }
    }

    candidate.bug_id = crate::ids::bug_id()?;
    candidate.opened_at = now()?;
    candidate.last_seen_at = candidate.opened_at.clone();
    atomic_write(&directory, &candidate)?;
    Ok(to_result(candidate, false))
}

/// List valid open records in stable identifier order.
///
/// A missing directory is an empty registry. Malformed, oversized, symlinked,
/// and non-regular entries do not hide unrelated valid bug reports.
pub fn list_open(directory: &Path) -> Result<Vec<results::BugRecord>, BugError> {
    let Some(directory) = open_store(directory, false)? else {
        return Ok(Vec::new());
    };
    lock(&directory, FlockOperation::LockShared, "lock for list")?;
    Ok(read_open_records(&directory)?
        .into_iter()
        .map(|record| to_result(record, false))
        .collect())
}

/// Close exactly one validated bug record and remove no other entry.
pub fn close(directory: &Path, bug_id: &str) -> Result<results::BugClosed, BugError> {
    if !valid_bug_id(bug_id) {
        return Err(BugError::Invalid("invalid bug id".into()));
    }
    let Some(directory) = open_store(directory, false)? else {
        return Err(no_open_bug(bug_id));
    };
    lock(&directory, FlockOperation::LockExclusive, "lock for close")?;

    let name = format!("{bug_id}.json");
    let opened = match open_regular(&directory, &name) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            return Err(no_open_bug(bug_id));
        }
        Err(source) => {
            return Err(BugError::Store {
                operation: "open exact bug record",
                source,
            });
        }
    };
    let expected_identity = file_identity(&opened, "inspect exact bug record")?;
    let record = read_record(opened)
        .filter(|record| record.bug_id == bug_id && validate_stored(record).is_ok())
        .ok_or_else(|| BugError::Invalid(format!("open bug {bug_id} is malformed")))?;

    // Moving before unlinking lets us revalidate the opened inode after the
    // only pathname transition, closing the path-swap window in a writable
    // store. A mismatched entry is restored when the original name is free.
    let quarantine = quarantine_name(bug_id)?;
    unix_fs::renameat_with(
        &directory,
        name.as_str(),
        &directory,
        quarantine.as_str(),
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            no_open_bug(bug_id)
        } else {
            store_error("quarantine exact bug record", error)
        }
    })?;
    let moved = open_regular(&directory, &quarantine).map_err(|source| BugError::Store {
        operation: "reopen quarantined bug record",
        source,
    })?;
    if file_identity(&moved, "revalidate quarantined bug record")? != expected_identity {
        let _ = unix_fs::renameat_with(
            &directory,
            quarantine.as_str(),
            &directory,
            name.as_str(),
            RenameFlags::NOREPLACE,
        );
        return Err(BugError::Invalid(format!(
            "open bug {bug_id} changed while it was being closed"
        )));
    }
    unix_fs::unlinkat(&directory, quarantine.as_str(), AtFlags::empty())
        .map_err(|error| store_error("remove exact bug record", error))?;
    directory.sync_all().map_err(|source| BugError::Store {
        operation: "sync after close",
        source,
    })?;
    Ok(results::BugClosed {
        bug_id: bug_id.to_owned(),
        closed: true,
        component: Some(record.component),
        summary: Some(record.summary),
        notified: None,
    })
}

fn clean(field: &str, value: &str, maximum: usize) -> Result<String, BugError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(BugError::Invalid(format!("'{field}' is required")));
    }
    validate_text(field, text, maximum)?;
    Ok(text.to_owned())
}

fn validate_text(field: &str, text: &str, maximum: usize) -> Result<(), BugError> {
    if text.chars().count() > maximum {
        return Err(BugError::Invalid(format!(
            "'{field}' exceeds {maximum} characters; keep records atomic, reference logs by path instead of pasting them"
        )));
    }
    if FORBIDDEN_SECRET.is_match(text) {
        return Err(BugError::Invalid(format!(
            "'{field}' looks like it contains a secret"
        )));
    }
    if PRIVATE_PATH.is_match(text) {
        return Err(BugError::Invalid(format!(
            "'{field}' contains a private host path; describe the location abstractly"
        )));
    }
    if looks_like_raw_log(text) {
        return Err(BugError::Invalid(format!(
            "'{field}' looks like a raw log; summarize it and reference bounded evidence instead"
        )));
    }
    Ok(())
}

fn looks_like_raw_log(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() < 4 {
        return false;
    }
    if text.contains("Traceback (most recent call last):") {
        return true;
    }
    let signals = lines
        .iter()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("at ")
                || (line.starts_with("File \"") && line.contains(", line "))
                || starts_with_timestamp(line)
                || [
                    "TRACE ", "DEBUG ", "INFO ", "WARN ", "WARNING ", "ERROR ", "FATAL ",
                ]
                .iter()
                .any(|level| line.starts_with(level))
        })
        .count();
    signals >= 4 && signals * 2 >= lines.len()
}

fn starts_with_timestamp(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.len() >= 11
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && matches!(bytes[10], b'T' | b' ')
}

fn validated_correlations(
    correlations: &params::BugCorrelations,
) -> Result<results::BugCorrelations, BugError> {
    Ok(results::BugCorrelations {
        run_id: correlation("run_id", correlations.run_id.as_deref())?,
        deployment_id: correlation("deployment_id", correlations.deployment_id.as_deref())?,
        component: correlation("component", correlations.component.as_deref())?,
        repository_id: correlation("repository_id", correlations.repository_id.as_deref())?,
        call_id: correlation("call_id", correlations.call_id.as_deref())?,
    })
}

fn correlation(field: &str, value: Option<&str>) -> Result<Option<String>, BugError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.chars().count() > MAX_CORRELATION_CHARS {
        return Err(BugError::Invalid(format!(
            "correlation {field:?} exceeds {MAX_CORRELATION_CHARS} characters"
        )));
    }
    validate_text(
        &format!("correlations.{field}"),
        value,
        MAX_CORRELATION_CHARS,
    )?;
    Ok(Some(value.to_owned()))
}

fn merge_correlations(
    existing: &mut results::BugCorrelations,
    incoming: &results::BugCorrelations,
) {
    if incoming.run_id.is_some() {
        existing.run_id.clone_from(&incoming.run_id);
    }
    if incoming.deployment_id.is_some() {
        existing.deployment_id.clone_from(&incoming.deployment_id);
    }
    if incoming.component.is_some() {
        existing.component.clone_from(&incoming.component);
    }
    if incoming.repository_id.is_some() {
        existing.repository_id.clone_from(&incoming.repository_id);
    }
    if incoming.call_id.is_some() {
        existing.call_id.clone_from(&incoming.call_id);
    }
}

fn validate_stored(record: &StoredBugRecord) -> Result<(), BugError> {
    if !valid_bug_id(&record.bug_id)
        || record.occurrences == 0
        || record.opened_at.is_empty()
        || record.opened_at.len() > 32
        || record.last_seen_at.is_empty()
        || record.last_seen_at.len() > 32
        || record.reporter.chars().count() > MAX_REPORTER_CHARS
    {
        return Err(BugError::Invalid("stored bug record is malformed".into()));
    }
    clean("component", &record.component, MAX_COMPONENT_CHARS)?;
    clean("summary", &record.summary, MAX_SUMMARY_CHARS)?;
    clean("expected", &record.expected, MAX_EXPECTED_CHARS)?;
    clean("actual", &record.actual, MAX_ACTUAL_CHARS)?;
    clean("steps", &record.steps, MAX_STEPS_CHARS)?;
    validate_result_correlations(&record.correlations)?;
    Ok(())
}

fn validate_result_correlations(correlations: &results::BugCorrelations) -> Result<(), BugError> {
    for (field, value) in [
        ("run_id", correlations.run_id.as_deref()),
        ("deployment_id", correlations.deployment_id.as_deref()),
        ("component", correlations.component.as_deref()),
        ("repository_id", correlations.repository_id.as_deref()),
        ("call_id", correlations.call_id.as_deref()),
    ] {
        correlation(field, value)?;
    }
    Ok(())
}

fn to_result(record: StoredBugRecord, duplicate: bool) -> results::BugRecord {
    results::BugRecord {
        component: record.component,
        summary: record.summary,
        expected: record.expected,
        actual: record.actual,
        steps: record.steps,
        correlations: record.correlations,
        bug_id: record.bug_id,
        opened_at: record.opened_at,
        last_seen_at: record.last_seen_at,
        occurrences: record.occurrences,
        reporter: record.reporter,
        duplicate,
        notified: None,
    }
}

fn now() -> Result<String, BugError> {
    Ok(OffsetDateTime::now_utc().format(TIMESTAMP_FORMAT)?)
}

fn valid_bug_id(value: &str) -> bool {
    value.len() == 13
        && value.starts_with('b')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn no_open_bug(bug_id: &str) -> BugError {
    BugError::Invalid(format!("no open bug {bug_id}"))
}

fn open_store(path: &Path, create: bool) -> Result<Option<File>, BugError> {
    if path.as_os_str().is_empty() {
        return Err(BugError::Invalid("bug store path is empty".into()));
    }
    let anchor = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = unix_fs::open(
        anchor,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| store_error("open bug store anchor", error))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(BugError::Invalid(
                    "bug store path may not contain parent traversal".into(),
                ));
            }
        };
        if create {
            match unix_fs::mkdirat(&directory, name, Mode::from_raw_mode(0o777)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(store_error("create bug store directory", error)),
            }
        }
        directory = match unix_fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(descriptor) => File::from(descriptor),
            Err(error) if !create && error == rustix::io::Errno::NOENT => return Ok(None),
            Err(error) => return Err(store_error("open bug store directory", error)),
        };
    }
    Ok(Some(directory))
}

fn lock(directory: &File, operation: FlockOperation, label: &'static str) -> Result<(), BugError> {
    unix_fs::flock(directory, operation).map_err(|error| store_error(label, error))
}

fn read_open_records(directory: &File) -> Result<Vec<StoredBugRecord>, BugError> {
    let mut names = Vec::new();
    let mut entries = Dir::read_from(directory).map_err(|error| store_error("list", error))?;
    for entry in &mut entries {
        let entry = entry.map_err(|error| store_error("read directory", error))?;
        if let Ok(name) = std::str::from_utf8(entry.file_name().to_bytes())
            && bug_id_from_name(name).is_some()
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    let mut records = Vec::new();
    for name in names {
        let Some(expected_id) = bug_id_from_name(&name) else {
            continue;
        };
        let Ok(file) = open_regular(directory, &name) else {
            continue;
        };
        let Some(record) = read_record(file) else {
            continue;
        };
        if record.bug_id == expected_id && validate_stored(&record).is_ok() {
            records.push(record);
        }
    }
    Ok(records)
}

fn bug_id_from_name(name: &str) -> Option<&str> {
    let id = name.strip_suffix(".json")?;
    valid_bug_id(id).then_some(id)
}

fn open_regular(directory: &File, name: &str) -> io::Result<File> {
    let file = unix_fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)?;
    let details = file.metadata()?;
    if !details.is_file() || details.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bug record is not a bounded regular file",
        ));
    }
    Ok(file)
}

fn read_record(file: File) -> Option<StoredBugRecord> {
    let mut payload = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut payload)
        .ok()?;
    if payload.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    serde_json::from_slice(&payload).ok()
}

fn atomic_write(directory: &File, record: &StoredBugRecord) -> Result<(), BugError> {
    let mut payload = serde_json::to_vec_pretty(record)?;
    payload.push(b'\n');
    if payload.len() as u64 > MAX_RECORD_BYTES {
        return Err(BugError::Invalid(
            "encoded bug record exceeds the atomic record limit".into(),
        ));
    }
    let target = format!("{}.json", record.bug_id);
    let temporary = temporary_name()?;
    let result = (|| -> Result<(), BugError> {
        let descriptor = unix_fs::openat(
            directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| store_error("create temporary bug record", error))?;
        let mut file = File::from(descriptor);
        file.write_all(&payload).map_err(|source| BugError::Store {
            operation: "write temporary bug record",
            source,
        })?;
        unix_fs::fchmod(&file, Mode::from_raw_mode(0o666))
            .map_err(|error| store_error("set shared bug record mode", error))?;
        file.sync_all().map_err(|source| BugError::Store {
            operation: "sync temporary bug record",
            source,
        })?;
        unix_fs::renameat(directory, temporary.as_str(), directory, target.as_str())
            .map_err(|error| store_error("publish atomic bug record", error))?;
        directory.sync_all().map_err(|source| BugError::Store {
            operation: "sync published bug record",
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unix_fs::unlinkat(directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn temporary_name() -> Result<String, BugError> {
    Ok(format!(".bug-{}.tmp", crate::ids::bug_id()?))
}

fn quarantine_name(bug_id: &str) -> Result<String, BugError> {
    Ok(format!(".closing-{bug_id}-{}", crate::ids::bug_id()?))
}

fn file_identity(file: &File, operation: &'static str) -> Result<(u64, u64), BugError> {
    let details = unix_fs::fstat(file).map_err(|error| store_error(operation, error))?;
    Ok((details.st_dev as _, details.st_ino))
}

fn store_error(operation: &'static str, error: rustix::io::Errno) -> BugError {
    BugError::Store {
        operation,
        source: error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn request(summary: &str) -> params::BugReport {
        params::BugReport {
            component: "api".into(),
            summary: summary.into(),
            expected: "200".into(),
            actual: "500".into(),
            steps: "GET /x".into(),
            correlations: params::BugCorrelations {
                deployment_id: Some("d1".into()),
                ..params::BugCorrelations::default()
            },
        }
    }

    #[test]
    fn report_coalesces_lists_and_closes_exact_record() {
        let temporary = tempdir().unwrap();
        let store = temporary.path().join("bugs");
        std::fs::create_dir(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o777)).unwrap();
        let first = report(&store, &request("500 on /x"), "uid:1000").unwrap();
        assert!(!first.duplicate && first.occurrences == 1);
        assert_eq!(first.bug_id.len(), 13);
        assert!(valid_bug_id(&first.bug_id));
        assert_eq!(
            std::fs::metadata(store.join(format!("{}.json", first.bug_id)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o666
        );

        let mut recurrence = request("500 on /x");
        recurrence.correlations.deployment_id = None;
        recurrence.correlations.run_id = Some("t1".into());
        let second = report(&store, &recurrence, "uid:1001").unwrap();
        assert!(second.duplicate && second.occurrences == 2);
        assert_eq!(second.reporter, "uid:1000");
        assert_eq!(second.correlations.deployment_id.as_deref(), Some("d1"));
        assert_eq!(second.correlations.run_id.as_deref(), Some("t1"));

        let listed = list_open(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].duplicate);
        let encoded = serde_json::to_value(&listed[0].correlations).unwrap();
        assert!(
            encoded
                .get("call_id")
                .is_some_and(serde_json::Value::is_null)
        );

        let closed = close(&store, &first.bug_id).unwrap();
        assert!(closed.closed);
        assert_eq!(closed.component.as_deref(), Some("api"));
        assert!(list_open(&store).unwrap().is_empty());
        assert!(
            close(&store, &first.bug_id)
                .unwrap_err()
                .to_string()
                .contains("no open bug")
        );
    }

    #[test]
    fn concurrent_recurrences_are_counted_once_each() {
        let temporary = tempdir().unwrap();
        let store = Arc::new(temporary.path().join("bugs"));
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || report(&store, &request("race"), "uid:1").unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let records = list_open(&store).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].occurrences, 12);
    }

    #[test]
    fn rejects_unbounded_sensitive_private_and_raw_text() {
        let temporary = tempdir().unwrap();
        for (report_request, expected_error) in [
            (
                {
                    let mut value = request("leak");
                    value.actual = "password=abc".into();
                    value
                },
                "secret",
            ),
            (
                {
                    let mut value = request("private");
                    value.actual = "see /home/someone/output.txt".into();
                    value
                },
                "private host path",
            ),
            (
                {
                    let mut value = request("large");
                    value.actual = "y".repeat(MAX_ACTUAL_CHARS + 1);
                    value
                },
                "exceeds",
            ),
            (
                {
                    let mut value = request("raw");
                    value.actual = (0..5)
                        .map(|second| format!("2026-09-03T00:00:0{second}Z ERROR failed"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    value
                },
                "raw log",
            ),
            (
                {
                    let mut value = request("correlation");
                    value.correlations.call_id = Some("x".repeat(MAX_CORRELATION_CHARS + 1));
                    value
                },
                "correlation",
            ),
        ] {
            let error = report(temporary.path(), &report_request, "uid:1").unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
        }
    }

    #[test]
    fn refuses_symlinks_special_entries_and_mismatched_records() {
        let temporary = tempdir().unwrap();
        let victim = temporary.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        let symlink_store = temporary.path().join("linked");
        symlink(&victim, &symlink_store).unwrap();
        assert!(report(&symlink_store, &request("x"), "uid:1").is_err());
        assert!(std::fs::read_dir(&victim).unwrap().next().is_none());

        let store = temporary.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let target = temporary.path().join("outside.json");
        std::fs::write(&target, b"outside").unwrap();
        let id = "b0123456789ab";
        symlink(&target, store.join(format!("{id}.json"))).unwrap();
        assert!(list_open(&store).unwrap().is_empty());
        assert!(close(&store, id).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside");

        let real = report(&store, &request("real"), "uid:1").unwrap();
        let record_path = store.join(format!("{}.json", real.bug_id));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
        value["bug_id"] = serde_json::Value::String("bffffffffffff".into());
        std::fs::write(&record_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(close(&store, &real.bug_id).is_err());
        assert!(record_path.exists());
        assert!(close(&store, "b0123456789ab/../../victim").is_err());
    }
}
