//! Caller-identity Git boundary for managed deployment generations.
//!
//! Git reads caller-controlled repository configuration, so the root daemon
//! never invokes it as root on behalf of a non-root caller. Every production
//! command is an explicit argument vector, has a bounded deadline, and keeps
//! only bounded diagnostic output.

use std::ffi::{CStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

const EXEC_PATH: &str = "/usr/bin:/bin";
const POLL: Duration = Duration::from_millis(10);
const OUTPUT_CAP: usize = 64 * 1024;
const ERROR_CAP: usize = 2 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSnapshot {
    pub commit: Option<String>,
    pub dirty: bool,
    pub source_digest: String,
}

#[derive(Debug, Error)]
pub enum DeploymentGitError {
    #[error("invalid deployment Git request: {0}")]
    Invalid(String),
    #[error("Git invocation failed: {0}")]
    Invocation(String),
    #[error("Git command failed: {0}")]
    Command(String),
}

pub trait DeploymentGit: Send + Sync + 'static {
    fn snapshot(
        &self,
        worktree: &Path,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<GitSnapshot, DeploymentGitError>;

    fn add_detached_worktree(
        &self,
        worktree: &Path,
        target: &Path,
        commit: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentGitError>;

    fn remove_worktree(
        &self,
        worktree: &Path,
        target: &Path,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentGitError>;

    fn is_ignored(
        &self,
        worktree: &Path,
        relative_path: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<bool, DeploymentGitError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GitCli;

impl DeploymentGit for GitCli {
    fn snapshot(
        &self,
        worktree: &Path,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<GitSnapshot, DeploymentGitError> {
        validate_worktree(worktree)?;
        let revision = run_git(
            worktree,
            caller_uid,
            caller_gid,
            &[OsString::from("rev-parse"), OsString::from("HEAD")],
            Duration::from_secs(30),
        )?;
        let commit = if revision.status.success() && !revision.stdout_truncated {
            let value = String::from_utf8_lossy(&revision.stdout).trim().to_owned();
            valid_commit(&value).then_some(value)
        } else {
            None
        };
        let status = run_git(
            worktree,
            caller_uid,
            caller_gid,
            &[OsString::from("status"), OsString::from("--porcelain")],
            Duration::from_secs(30),
        )?;
        Ok(GitSnapshot {
            commit,
            dirty: !status.status.success() || status.stdout_truncated || !status.stdout.is_empty(),
            source_digest: crate::test_command::TestCommand::source_digest(
                &crate::test_command::HostTestCommand,
                &crate::test_lifecycle::default_executor_path().map_err(|_| {
                    DeploymentGitError::Command(
                        "cannot resolve the deployment source-digest executor".into(),
                    )
                })?,
                worktree,
                caller_uid,
                caller_gid,
            )
            .map_err(|_| {
                DeploymentGitError::Command("cannot establish the deployment source digest".into())
            })?,
        })
    }

    fn add_detached_worktree(
        &self,
        worktree: &Path,
        target: &Path,
        commit: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentGitError> {
        validate_worktree(worktree)?;
        validate_target(target)?;
        if !valid_commit(commit) {
            return Err(DeploymentGitError::Invalid(
                "commit must be one full hexadecimal object ID".into(),
            ));
        }
        let output = run_git(
            worktree,
            caller_uid,
            caller_gid,
            &[
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                target.as_os_str().to_owned(),
                OsString::from(commit),
            ],
            Duration::from_secs(300),
        )?;
        require_success(output, "git worktree add failed")
    }

    fn remove_worktree(
        &self,
        worktree: &Path,
        target: &Path,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<(), DeploymentGitError> {
        validate_worktree(worktree)?;
        validate_target(target)?;
        let output = run_git(
            worktree,
            caller_uid,
            caller_gid,
            &[
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                target.as_os_str().to_owned(),
            ],
            Duration::from_secs(120),
        )?;
        require_success(output, "git worktree remove failed")
    }

    fn is_ignored(
        &self,
        worktree: &Path,
        relative_path: &str,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<bool, DeploymentGitError> {
        validate_worktree(worktree)?;
        if relative_path.is_empty()
            || Path::new(relative_path).is_absolute()
            || Path::new(relative_path)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(DeploymentGitError::Invalid(
                "ignored-file query requires a normalized relative path".into(),
            ));
        }
        let output = run_git(
            worktree,
            caller_uid,
            caller_gid,
            &[
                OsString::from("check-ignore"),
                OsString::from("--quiet"),
                OsString::from("--"),
                OsString::from(relative_path),
            ],
            Duration::from_secs(30),
        )?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(DeploymentGitError::Command(if output.stderr_truncated {
                "git check-ignore failed".into()
            } else {
                bounded_text(&String::from_utf8_lossy(&output.stderr), ERROR_CAP)
            })),
        }
    }
}

fn validate_worktree(path: &Path) -> Result<(), DeploymentGitError> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(DeploymentGitError::Invalid(
            "worktree must be an existing absolute directory".into(),
        ));
    }
    Ok(())
}

fn validate_target(path: &Path) -> Result<(), DeploymentGitError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(DeploymentGitError::Invalid(
            "generation target must be an absolute normalized path".into(),
        ));
    }
    Ok(())
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

fn run_git(
    worktree: &Path,
    caller_uid: u32,
    caller_gid: u32,
    arguments: &[OsString],
    timeout: Duration,
) -> Result<CommandOutput, DeploymentGitError> {
    let git = trusted_executable("git").map_err(invocation_error)?;
    let mut command = if effective_uid() == 0 && caller_uid != 0 {
        let home = passwd_home(caller_uid).map_err(invocation_error)?;
        let setpriv = trusted_executable("setpriv").map_err(invocation_error)?;
        let mut command = Command::new(setpriv);
        command
            .arg(format!("--reuid={caller_uid}"))
            .arg(format!("--regid={caller_gid}"))
            .arg("--init-groups")
            .arg("--")
            .arg(&git)
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env("HOME", home);
        command
    } else {
        let mut command = Command::new(git);
        command
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env(
                "HOME",
                std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/nonexistent")),
            )
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "safe.directory")
            .env("GIT_CONFIG_VALUE_0", "*");
        command
    };
    command
        .arg("-C")
        .arg(worktree)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_bounded(command, timeout).map_err(invocation_error)
}

fn run_bounded(mut command: Command, timeout: Duration) -> io::Result<CommandOutput> {
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git stdout pipe was not created"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Git stderr pipe was not created"))?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Git command timed out after {} seconds", timeout.as_secs()),
            ));
        }
        thread::sleep(POLL.min(deadline.saturating_duration_since(now)));
    };
    let (stdout, stdout_truncated) = stdout_reader
        .join()
        .map_err(|_| io::Error::other("Git stdout reader failed"))??;
    let (stderr, stderr_truncated) = stderr_reader
        .join()
        .map_err(|_| io::Error::other("Git stderr reader failed"))??;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn read_bounded(mut input: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::with_capacity(4_096);
    let mut buffer = [0_u8; 4_096];
    let mut truncated = false;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = OUTPUT_CAP.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((kept, truncated))
}

fn require_success(output: CommandOutput, fallback: &str) -> Result<(), DeploymentGitError> {
    if output.status.success() {
        return Ok(());
    }
    let detail = if output.stderr_truncated {
        fallback.to_owned()
    } else {
        bounded_text(&String::from_utf8_lossy(&output.stderr), ERROR_CAP)
    };
    Err(DeploymentGitError::Command(if detail.trim().is_empty() {
        fallback.to_owned()
    } else {
        detail
    }))
}

fn bounded_text(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn trusted_executable(name: &str) -> io::Result<PathBuf> {
    for directory in ["/usr/bin", "/bin"] {
        let candidate = Path::new(directory).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name} was not found in {EXEC_PATH}"),
    ))
}

fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn passwd_home(uid: u32) -> io::Result<OsString> {
    let suggested =
        // SAFETY: `sysconf` has no pointer arguments and the selector is valid.
        unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    };
    let mut storage = vec![0_u8; size.max(1_024)];
    // SAFETY: A zeroed passwd value is a valid output buffer for getpwuid_r.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: All pointers refer to live writable storage of the stated size.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut result,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if result.is_null() || entry.pw_dir.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown caller uid {uid}"),
        ));
    }
    // SAFETY: Successful getpwuid_r returned a NUL-terminated pointer backed
    // by `storage`, which stays alive until this copy completes.
    let bytes = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes().to_vec();
    Ok(OsString::from_vec(bytes))
}

fn invocation_error(error: io::Error) -> DeploymentGitError {
    DeploymentGitError::Invocation(bounded_text(&error.to_string(), ERROR_CAP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env("HOME", "/nonexistent")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn snapshot_and_detached_generation_preserve_dirty_semantics() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repository");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        fs::write(repository.join("tracked"), "one\n").unwrap();
        git(&repository, &["add", "tracked"]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let clean = GitCli.snapshot(&repository, uid, gid).unwrap();
        assert!(
            clean
                .commit
                .as_ref()
                .is_some_and(|value| valid_commit(value))
        );
        assert!(!clean.dirty);

        fs::write(repository.join("tracked"), "two\n").unwrap();
        assert!(GitCli.snapshot(&repository, uid, gid).unwrap().dirty);
        git(&repository, &["checkout", "--quiet", "--", "tracked"]);

        let generation = temporary.path().join("generation");
        GitCli
            .add_detached_worktree(
                &repository,
                &generation,
                clean.commit.as_deref().unwrap(),
                uid,
                gid,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(generation.join("tracked")).unwrap(),
            "one\n"
        );
        GitCli
            .remove_worktree(&repository, &generation, uid, gid)
            .unwrap();
        assert!(!generation.exists());
    }

    #[test]
    fn refuses_relative_targets_and_partial_object_ids() {
        let error = GitCli
            .add_detached_worktree(Path::new("/missing"), Path::new("relative"), "abc", 1, 1)
            .unwrap_err();
        assert!(matches!(error, DeploymentGitError::Invalid(_)));
        assert!(!valid_commit("abcd"));
        assert!(valid_commit(&"a".repeat(40)));
        assert!(valid_commit(&"b".repeat(64)));
    }
}
