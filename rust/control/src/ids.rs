//! Stable opaque identifiers and systemd unit names.

use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

const REPOSITORY_NAMESPACE: &[u8] = b"devcoordinator2.repository\0";
const WORKTREE_NAMESPACE: &[u8] = b"devcoordinator2.worktree\0";
const OBSERVED_DEPLOYMENT_NAMESPACE: &[u8] = b"devcoordinator2.observed-deployment\0";
const RUN_FORMAT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

#[derive(Debug, Error)]
pub enum IdError {
    #[error("cannot resolve identity path: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot obtain secure random bytes: {0}")]
    Random(#[from] getrandom::Error),
    #[error("cannot format timestamp: {0}")]
    Time(#[from] time::error::Format),
}

pub fn repository_id(git_common_root: &Path) -> Result<String, IdError> {
    Ok(format!(
        "r{}",
        path_digest(REPOSITORY_NAMESPACE, git_common_root)?
    ))
}

pub fn worktree_id(worktree_root: &Path) -> Result<String, IdError> {
    Ok(format!(
        "w{}",
        path_digest(WORKTREE_NAMESPACE, worktree_root)?
    ))
}

pub fn observed_deployment_id(repository_id: &str, native_project: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(OBSERVED_DEPLOYMENT_NAMESPACE);
    hasher.update(repository_id.as_bytes());
    hasher.update([0]);
    hasher.update(native_project.as_bytes());
    format!("d{}", first_hex(hasher.finalize().as_slice(), 8))
}

pub fn run_id(now: Option<OffsetDateTime>) -> Result<String, IdError> {
    Ok(format!(
        "t{}-{}",
        now.unwrap_or_else(OffsetDateTime::now_utc)
            .format(RUN_FORMAT)?,
        random_hex(3)?
    ))
}

pub fn task_id() -> Result<String, IdError> {
    random_id('p')
}

pub fn release_id() -> Result<String, IdError> {
    random_id('v')
}

pub fn decision_id() -> Result<String, IdError> {
    random_id('n')
}

pub fn glossary_id() -> Result<String, IdError> {
    random_id('g')
}

pub fn feedback_id() -> Result<String, IdError> {
    random_id('f')
}

pub fn comment_id() -> Result<String, IdError> {
    random_id('m')
}

pub fn bug_id() -> Result<String, IdError> {
    Ok(format!("b{}", random_hex(6)?))
}

pub fn lease_id() -> Result<String, IdError> {
    random_id('l')
}

pub fn unit_name(prefix: &str, worktree_id: &str, suffix: &str) -> String {
    format!("{prefix}-{worktree_id}-{suffix}.service")
}

pub fn unit_glob(prefix: &str, worktree_id: &str) -> String {
    format!("{prefix}-{worktree_id}-*.service")
}

fn path_digest(namespace: &[u8], path: &Path) -> Result<String, IdError> {
    let canonical = path.canonicalize()?;
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    Ok(first_hex(hasher.finalize().as_slice(), 8))
}

fn random_id(prefix: char) -> Result<String, IdError> {
    Ok(format!("{prefix}{}", random_hex(8)?))
}

fn random_hex(bytes: usize) -> Result<String, IdError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value)?;
    Ok(first_hex(&value, bytes))
}

fn first_hex(value: &[u8], bytes: usize) -> String {
    let mut result = String::with_capacity(bytes * 2);
    for byte in value.iter().take(bytes) {
        use std::fmt::Write;
        write!(result, "{byte:02x}").expect("String writes cannot fail");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    #[test]
    fn deterministic_ids_match_the_established_namespaces() {
        let temporary = tempdir().expect("tempdir");
        let first = repository_id(temporary.path()).expect("repository id");
        let second = repository_id(&temporary.path().join(".")).expect("repository id");
        assert_eq!(first, second);
        assert!(first.starts_with('r') && first.len() == 17);
        assert_ne!(first[1..], worktree_id(temporary.path()).unwrap()[1..]);
    }

    #[test]
    fn random_and_time_ordered_ids_keep_the_public_shapes() {
        assert_eq!(
            &run_id(Some(datetime!(2026-09-03 12:34:56 UTC))).unwrap()[..17],
            "t20260903T123456Z"
        );
        for (prefix, value) in [
            ('p', task_id().unwrap()),
            ('v', release_id().unwrap()),
            ('n', decision_id().unwrap()),
            ('f', feedback_id().unwrap()),
            ('m', comment_id().unwrap()),
        ] {
            assert!(value.starts_with(prefix));
            assert_eq!(value.len(), 17);
        }
    }
}
