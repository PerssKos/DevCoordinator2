//! Docker and Compose command boundary.
//!
//! The production adapter executes one explicit argv vector with a cleared
//! environment. Managed labels are generated inside this module, container
//! mutations require a full 64-hex ID, and cleanup is always exact-targeted.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

pub const LABEL_PREFIX: &str = "devcoordinator2";
pub const DEFAULT_DOCKER_TIMEOUT: Duration = Duration::from_secs(120);
pub const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const MAX_LOG_BYTES: usize = 64 * 1024;
const READINESS_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockerErrorKind {
    CliUnavailable,
    SpawnFailed,
    TimedOut,
    Cancelled,
    CommandFailed,
    InvalidOutput,
    InvalidTarget,
    InvalidRequest,
}

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker CLI not installed")]
    CliUnavailable,
    #[error("{operation}: {source}")]
    Spawn {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("docker {operation} timed out")]
    Timeout { operation: String },
    #[error("docker {operation} cancelled")]
    Cancelled { operation: String },
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    InvalidOutput(String),
    #[error("{0}")]
    InvalidTarget(String),
    #[error("{0}")]
    InvalidRequest(String),
}

impl DockerError {
    pub const fn kind(&self) -> DockerErrorKind {
        match self {
            Self::CliUnavailable => DockerErrorKind::CliUnavailable,
            Self::Spawn { .. } => DockerErrorKind::SpawnFailed,
            Self::Timeout { .. } => DockerErrorKind::TimedOut,
            Self::Cancelled { .. } => DockerErrorKind::Cancelled,
            Self::Command(_) => DockerErrorKind::CommandFailed,
            Self::InvalidOutput(_) => DockerErrorKind::InvalidOutput,
            Self::InvalidTarget(_) => DockerErrorKind::InvalidTarget,
            Self::InvalidRequest(_) => DockerErrorKind::InvalidRequest,
        }
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExactContainerId(String);

impl ExactContainerId {
    pub fn parse(value: impl Into<String>) -> Result<Self, DockerError> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DockerError::InvalidTarget(
                "container reference must be one exact full 64-hex ID".to_owned(),
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ExactContainerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Display for ExactContainerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for ExactContainerId {
    type Error = DockerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct ManagedVolumeName(String);

impl ManagedVolumeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ManagedVolumeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Display for ManagedVolumeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub fn container_name(deployment_id: &str, component: &str) -> String {
    format!("devcoordinator2-deploy-{deployment_id}-{component}")
}

pub fn managed_volume_name(
    deployment_id: &str,
    component: &str,
    declared: &str,
) -> ManagedVolumeName {
    ManagedVolumeName(format!(
        "devcoordinator2-{deployment_id}-{component}-{declared}"
    ))
}

pub fn compose_project(deployment_id: &str, component: &str) -> String {
    format!("dc2-{deployment_id}-{component}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedLabelContext {
    pub instance: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub run_id: Option<String>,
    pub deployment_id: Option<String>,
    pub component: Option<String>,
    pub generation: Option<u64>,
    pub ttl_seconds: Option<u64>,
    pub purpose: String,
    pub caller_uid: u32,
    pub client: String,
    pub session: Option<String>,
    pub created_at: String,
    pub data_class: String,
}

pub fn managed_labels(
    context: &ManagedLabelContext,
    caller_labels: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, DockerError> {
    if caller_labels
        .keys()
        .any(|key| key == LABEL_PREFIX || key.starts_with(&format!("{LABEL_PREFIX}.")))
    {
        return Err(DockerError::InvalidRequest(
            "caller labels may not override managed DevCoordinator labels".to_owned(),
        ));
    }
    let mut labels = caller_labels.clone();
    let mut insert = |name: &str, value: String| {
        labels.insert(format!("{LABEL_PREFIX}.{name}"), value);
    };
    insert("instance", context.instance.clone());
    insert("repository", context.repository_id.clone());
    insert("worktree", context.worktree_id.clone());
    if let Some(run_id) = &context.run_id {
        insert("run", run_id.clone());
    }
    if let Some(deployment_id) = &context.deployment_id {
        insert("deployment", deployment_id.clone());
    }
    if let Some(component) = &context.component {
        insert("component", component.clone());
    }
    if let Some(generation) = context.generation {
        insert("generation", generation.to_string());
    }
    if let Some(ttl_seconds) = context.ttl_seconds {
        insert("ttl_seconds", ttl_seconds.to_string());
    }
    insert("purpose", context.purpose.clone());
    insert("caller_uid", context.caller_uid.to_string());
    insert("client", context.client.clone());
    if let Some(session) = context.session.as_ref().filter(|value| !value.is_empty()) {
        insert("session", session.chars().take(128).collect());
    }
    insert("created", context.created_at.clone());
    insert("data", context.data_class.clone());
    Ok(labels)
}

/// One process request. This type deliberately has no `Debug` implementation:
/// its environment may contain secret values.
pub struct DockerInvocation {
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    output_log: Option<Arc<Mutex<File>>>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl DockerInvocation {
    pub fn new(args: Vec<OsString>, timeout: Duration) -> Result<Self, DockerError> {
        if args.is_empty() {
            return Err(DockerError::InvalidRequest(
                "Docker argv must contain an operation".to_owned(),
            ));
        }
        Ok(Self {
            args,
            cwd: None,
            environment: BTreeMap::new(),
            timeout,
            output_log: None,
            cancellation: None,
        })
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_output_log(mut self, output_log: Option<Arc<Mutex<File>>>) -> Self {
        self.output_log = output_log;
        self
    }

    pub fn with_cancellation(mut self, cancellation: Option<Arc<AtomicBool>>) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_environment(mut self, environment: BTreeMap<OsString, OsString>) -> Self {
        self.environment = environment;
        self
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

pub struct DockerOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl DockerOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

fn capture_stream(
    mut source: impl Read,
    output_log: Option<Arc<Mutex<File>>>,
) -> io::Result<CapturedStream> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = source.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        if let Some(log) = &output_log {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_all(&chunk[..read])?;
        }
        retained.extend_from_slice(&chunk[..read]);
        if retained.len() > MAX_CAPTURE_BYTES {
            let excess = retained.len() - MAX_CAPTURE_BYTES;
            retained.drain(..excess);
            truncated = true;
        }
    }
    Ok(CapturedStream {
        bytes: retained,
        truncated,
    })
}

fn operation_name(args: &[OsString]) -> String {
    args.first()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "command".to_owned())
}

fn execute_process(
    executable: &Path,
    invocation: DockerInvocation,
) -> Result<DockerOutput, DockerError> {
    if invocation
        .environment
        .keys()
        .any(|name| name == OsStr::new("PATH"))
    {
        return Err(DockerError::InvalidRequest(
            "Docker command environment may not override the fixed PATH".to_owned(),
        ));
    }
    let operation = operation_name(&invocation.args);
    if invocation
        .cancellation
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        return Err(DockerError::Cancelled { operation });
    }
    let mut command = Command::new(executable);
    command
        .args(&invocation.args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    if let Some(cwd) = invocation.cwd {
        command.current_dir(cwd);
    }
    command.envs(invocation.environment);
    let mut child = command.spawn().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            DockerError::CliUnavailable
        } else {
            DockerError::Spawn {
                operation: "start docker command",
                source,
            }
        }
    })?;
    let stdout = child.stdout.take().ok_or_else(|| DockerError::Spawn {
        operation: "capture docker stdout",
        source: io::Error::other("stdout pipe was not created"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| DockerError::Spawn {
        operation: "capture docker stderr",
        source: io::Error::other("stderr pipe was not created"),
    })?;
    let stdout_log = invocation.output_log.clone();
    let stderr_log = invocation.output_log;
    let stdout_reader = thread::spawn(move || capture_stream(stdout, stdout_log));
    let stderr_reader = thread::spawn(move || capture_stream(stderr, stderr_log));
    let pid = child.id();
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        // Observe completion without reaping. The owner below keeps the child
        // PID/process-group identity reserved through cancellation and cleanup.
        let result = loop {
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                break Err(error);
            }
        };
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + invocation.timeout;
    let status = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let interval = if invocation.cancellation.is_some() {
            remaining.min(READINESS_POLL)
        } else {
            remaining
        };
        match receiver.recv_timeout(interval) {
            Ok(result) => {
                result.map_err(|source| DockerError::Spawn {
                    operation: "wait for docker command",
                    source,
                })?;
                break child.wait().map_err(|source| DockerError::Spawn {
                    operation: "reap docker command",
                    source,
                })?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let cancelled = invocation
                    .cancellation
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire));
                if !cancelled && Instant::now() < deadline {
                    continue;
                }
                // SAFETY: this PID belongs to the unreaped child moved into the
                // waiter thread. Sending SIGKILL cannot target a reused PID while
                // that child still exists or remains a zombie.
                unsafe {
                    libc::kill(-pid.cast_signed(), libc::SIGKILL);
                }
                let _ = receiver.recv();
                let _ = waiter.join();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(if cancelled {
                    DockerError::Cancelled { operation }
                } else {
                    DockerError::Timeout { operation }
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The waiter disappeared before reporting a status. Preserve the
                // same exact child target and make a best effort to terminate it;
                // the owner has not reaped the Child, so the group identity is
                // still reserved even if the observer thread failed.
                unsafe {
                    libc::kill(-pid.cast_signed(), libc::SIGKILL);
                }
                let _ = waiter.join();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(DockerError::Spawn {
                    operation: "wait for docker command",
                    source: io::Error::other("Docker waiter stopped unexpectedly"),
                });
            }
        }
    };
    waiter.join().map_err(|_| DockerError::Spawn {
        operation: "join docker waiter",
        source: io::Error::other("Docker waiter panicked"),
    })?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| DockerError::Spawn {
            operation: "join docker stdout reader",
            source: io::Error::other("Docker stdout reader panicked"),
        })?
        .map_err(|source| DockerError::Spawn {
            operation: "read docker stdout",
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| DockerError::Spawn {
            operation: "join docker stderr reader",
            source: io::Error::other("Docker stderr reader panicked"),
        })?
        .map_err(|source| DockerError::Spawn {
            operation: "read docker stderr",
            source,
        })?;
    Ok(DockerOutput {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8(stdout.bytes).map_err(|_| {
            DockerError::InvalidOutput("docker stdout was not valid UTF-8".to_owned())
        })?,
        stderr: String::from_utf8(stderr.bytes).map_err(|_| {
            DockerError::InvalidOutput("docker stderr was not valid UTF-8".to_owned())
        })?,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

pub struct LogFollower {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl LogFollower {
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        // SAFETY: the PID is owned by `self.child`, which has not been reaped.
        let result = unsafe { libc::kill(self.child.id().cast_signed(), libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

impl Drop for LogFollower {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Clone, Debug)]
pub struct DockerCli {
    executable: PathBuf,
}

impl Default for DockerCli {
    fn default() -> Self {
        Self::system()
    }
}

impl DockerCli {
    pub fn system() -> Self {
        Self {
            executable: PathBuf::from("/usr/bin/docker"),
        }
    }

    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

fn spawn_log_process(
    executable: &Path,
    container_id: &ExactContainerId,
) -> Result<LogFollower, DockerError> {
    let mut child = Command::new(executable)
        .args(["logs", "--follow", container_id.as_str()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                DockerError::CliUnavailable
            } else {
                DockerError::Spawn {
                    operation: "docker logs",
                    source,
                }
            }
        })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok(LogFollower {
        child,
        stdout,
        stderr,
    })
}

pub struct RunDetachedRequest {
    pub name: String,
    pub image: String,
    pub label_context: ManagedLabelContext,
    pub labels: BTreeMap<String, String>,
    pub env_names: Vec<String>,
    pub env_values: BTreeMap<String, String>,
    pub publish: Vec<String>,
    pub tmpfs: Vec<String>,
    pub command: Vec<String>,
}

pub struct CreateContainerRequest {
    pub name: String,
    pub image: String,
    pub label_context: ManagedLabelContext,
    pub labels: BTreeMap<String, String>,
    pub env_file: Option<PathBuf>,
    pub publish: Vec<String>,
    pub volumes: Vec<String>,
    pub command: Vec<String>,
    pub restart: String,
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn append_labels(argv: &mut Vec<OsString>, labels: BTreeMap<String, String>) {
    for (key, value) in labels {
        argv.push("--label".into());
        argv.push(format!("{key}={value}").into());
    }
}

fn bounded_prefix(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn bounded_tail(value: &str, limit: usize) -> String {
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

#[derive(Clone, Copy)]
enum PostgresLogEvent {
    Ready,
    Closed,
    Failed,
}

fn scan_postgres_ready(mut source: impl Read, sender: mpsc::Sender<PostgresLogEvent>) {
    const NEEDLE: &[u8] = b"database system is ready to accept connections";
    let mut carry = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => {
                let _ = sender.send(PostgresLogEvent::Failed);
                return;
            }
        };
        if read == 0 {
            let _ = sender.send(PostgresLogEvent::Closed);
            return;
        }
        carry.extend_from_slice(&buffer[..read]);
        for _ in carry
            .windows(NEEDLE.len())
            .filter(|window| *window == NEEDLE)
        {
            if sender.send(PostgresLogEvent::Ready).is_err() {
                return;
            }
        }
        let keep = NEEDLE.len().saturating_sub(1).min(carry.len());
        carry.drain(..carry.len() - keep);
    }
}

fn first_error(output: &DockerOutput, limit: usize, fallback: &str) -> DockerError {
    let detail = bounded_prefix(&output.stderr, limit);
    DockerError::Command(if detail.is_empty() {
        fallback.to_owned()
    } else {
        detail
    })
}

fn combined_tail_error(output: &DockerOutput, fallback: &str) -> DockerError {
    let detail = bounded_tail(
        &format!("{}\n{}", output.stdout, output.stderr),
        MAX_ERROR_BYTES,
    );
    DockerError::Command(if detail.is_empty() {
        fallback.to_owned()
    } else {
        detail
    })
}

fn digest_suffix(image: &str) -> Result<Option<&str>, DockerError> {
    let Some((_, suffix)) = image.rsplit_once('@') else {
        return Ok(None);
    };
    if !suffix.starts_with("sha256:") {
        return Ok(None);
    }
    let digest = &suffix["sha256:".len()..];
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DockerError::InvalidRequest(
            "immutable image reference must contain one full sha256 digest".to_owned(),
        ));
    }
    Ok(Some(suffix))
}

fn ensure_exact_output(output: &DockerOutput, operation: &str) -> Result<(), DockerError> {
    if output.stdout_truncated || output.stderr_truncated {
        return Err(DockerError::InvalidOutput(format!(
            "docker {operation} output exceeded the bounded capture"
        )));
    }
    Ok(())
}

pub trait DockerControl: Send + Sync {
    fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError>;

    fn spawn_follow_logs(
        &self,
        container_id: &ExactContainerId,
    ) -> Result<LogFollower, DockerError>;

    fn available(&self) -> bool {
        self.invoke(
            DockerInvocation::new(
                vec![
                    "version".into(),
                    "--format".into(),
                    "{{.Server.Version}}".into(),
                ],
                Duration::from_secs(15),
            )
            .expect("static Docker invocation is valid"),
        )
        .is_ok_and(|output| output.success())
    }

    fn ensure_digest_image(&self, image: &str) -> Result<(), DockerError> {
        let Some(requested) = digest_suffix(image)? else {
            return Ok(());
        };
        if image_has_digest(self, image, requested)? {
            return Ok(());
        }
        let output = self.invoke(
            DockerInvocation::new(
                vec!["pull".into(), "--quiet".into(), image.into()],
                IMAGE_PULL_TIMEOUT,
            )
            .expect("static Docker invocation is valid"),
        )?;
        if !output.success() {
            return Err(first_error(
                &output,
                1_024,
                "cannot pull the pinned database fixture image",
            ));
        }
        if !image_has_digest(self, image, requested)? {
            return Err(DockerError::Command(
                "pulled database fixture image did not resolve to the requested sha256 digest"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn ensure_image(&self, image: &str) -> Result<(), DockerError> {
        let inspected = self.invoke(
            DockerInvocation::new(
                vec!["image".into(), "inspect".into(), image.into()],
                Duration::from_secs(30),
            )
            .expect("static Docker invocation is valid"),
        )?;
        if inspected.success() {
            return Ok(());
        }
        let pulled = self.invoke(
            DockerInvocation::new(vec!["pull".into(), image.into()], IMAGE_PULL_TIMEOUT)
                .expect("static Docker invocation is valid"),
        )?;
        if pulled.success() {
            Ok(())
        } else {
            Err(first_error(&pulled, 1_024, &format!("cannot pull {image}")))
        }
    }

    fn run_detached(&self, request: &RunDetachedRequest) -> Result<ExactContainerId, DockerError> {
        let labels = managed_labels(&request.label_context, &request.labels)?;
        let names = request.env_names.iter().cloned().collect::<BTreeSet<_>>();
        if names.len() != request.env_names.len()
            || names
                .iter()
                .any(|name| !valid_env_name(name) || name == "PATH")
            || request.env_values.keys().cloned().collect::<BTreeSet<_>>() != names
        {
            return Err(DockerError::InvalidRequest(
                "Docker environment names and values must be an exact safe set".to_owned(),
            ));
        }
        let mut argv = vec![
            "run".into(),
            "--detach".into(),
            "--name".into(),
            request.name.clone().into(),
            "--pull".into(),
            "never".into(),
        ];
        append_labels(&mut argv, labels);
        for name in &request.env_names {
            argv.push("--env".into());
            argv.push(name.into());
        }
        for publish in &request.publish {
            argv.push("--publish".into());
            argv.push(publish.into());
        }
        for tmpfs in &request.tmpfs {
            argv.push("--tmpfs".into());
            argv.push(tmpfs.into());
        }
        argv.push(request.image.clone().into());
        argv.extend(request.command.iter().map(OsString::from));
        let environment = request
            .env_values
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect();
        let output = self.invoke(
            DockerInvocation::new(argv, DEFAULT_DOCKER_TIMEOUT)?.with_environment(environment),
        )?;
        if !output.success() {
            // This command carries secret values in its environment. Docker's
            // diagnostics are not returned because a hostile executable could
            // echo them.
            return Err(DockerError::Command("docker run failed".to_owned()));
        }
        ensure_exact_output(&output, "run")?;
        ExactContainerId::parse(output.stdout.trim().to_owned()).map_err(|_| {
            DockerError::InvalidOutput(format!(
                "unexpected docker run output: {:?}",
                bounded_prefix(output.stdout.trim(), 80)
            ))
        })
    }

    fn create_container(
        &self,
        request: &CreateContainerRequest,
    ) -> Result<ExactContainerId, DockerError> {
        let labels = managed_labels(&request.label_context, &request.labels)?;
        let mut argv = vec![
            "create".into(),
            "--name".into(),
            request.name.clone().into(),
            format!("--restart={}", request.restart).into(),
        ];
        append_labels(&mut argv, labels);
        if let Some(env_file) = &request.env_file {
            argv.push("--env-file".into());
            argv.push(env_file.as_os_str().to_owned());
        }
        for publish in &request.publish {
            argv.push("--publish".into());
            argv.push(publish.into());
        }
        for volume in &request.volumes {
            argv.push("--volume".into());
            argv.push(volume.into());
        }
        argv.push(request.image.clone().into());
        argv.extend(request.command.iter().map(OsString::from));
        let output = self.invoke(DockerInvocation::new(argv, DEFAULT_DOCKER_TIMEOUT)?)?;
        if !output.success() {
            return Err(first_error(&output, 1_024, "docker create failed"));
        }
        ensure_exact_output(&output, "create")?;
        ExactContainerId::parse(output.stdout.trim().to_owned())
            .map_err(|_| DockerError::InvalidOutput("unexpected docker create output".to_owned()))
    }

    fn inspect(&self, container_id: &ExactContainerId) -> Result<Value, DockerError> {
        let output = self.invoke(DockerInvocation::new(
            vec![
                "inspect".into(),
                "--format".into(),
                "{{json .}}".into(),
                container_id.as_str().into(),
            ],
            Duration::from_secs(30),
        )?)?;
        if !output.success() {
            return Err(first_error(&output, 512, "docker inspect failed"));
        }
        ensure_exact_output(&output, "inspect")?;
        serde_json::from_str(output.stdout.trim()).map_err(|_| {
            DockerError::InvalidOutput("docker inspect returned invalid JSON".to_owned())
        })
    }

    fn published_host_port(
        &self,
        container_id: &ExactContainerId,
        container_port: &str,
    ) -> Result<u16, DockerError> {
        let info = self.inspect(container_id)?;
        let bindings = info
            .get("NetworkSettings")
            .and_then(|value| value.get("Ports"))
            .and_then(|value| value.get(container_port))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        bindings
            .iter()
            .filter_map(|binding| binding.get("HostPort").and_then(Value::as_str))
            .find_map(|port| port.parse::<u16>().ok())
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                DockerError::InvalidOutput(format!(
                    "container publishes no host port for {container_port}"
                ))
            })
    }

    fn exec_ok(&self, container_id: &ExactContainerId, argv: &[String], timeout: Duration) -> bool {
        let mut arguments = vec!["exec".into(), container_id.as_str().into()];
        arguments.extend(argv.iter().map(OsString::from));
        self.invoke(
            DockerInvocation::new(arguments, timeout).expect("docker exec always has an operation"),
        )
        .is_ok_and(|output| output.success())
    }

    fn postgres_facts(
        &self,
        container_id: &ExactContainerId,
        user: &str,
        database: &str,
    ) -> Result<Option<BTreeMap<String, u64>>, DockerError> {
        if user.is_empty()
            || database.is_empty()
            || !user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !database
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(DockerError::InvalidRequest(
                "PostgreSQL metric identity is invalid".into(),
            ));
        }
        const SQL: &str = "select (select count(*) from pg_stat_activity), (select coalesce(sum(size),0) from pg_ls_waldir()), (select coalesce(sum(temp_bytes),0) from pg_stat_database), (select coalesce(sum(pg_database_size(datname)),0) from pg_database where not datistemplate)";
        let output = self.invoke(DockerInvocation::new(
            vec![
                "exec".into(),
                container_id.as_str().into(),
                "psql".into(),
                "-U".into(),
                user.into(),
                "-d".into(),
                database.into(),
                "-tA".into(),
                "-F".into(),
                "|".into(),
                "-c".into(),
                SQL.into(),
            ],
            Duration::from_secs(20),
        )?)?;
        if !output.success() || output.stdout_truncated || output.stderr_truncated {
            return Ok(None);
        }
        let values = output
            .stdout
            .trim()
            .split('|')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>();
        let Ok(values) = values else {
            return Ok(None);
        };
        let [connections, wal, temporary, database] = values.as_slice() else {
            return Ok(None);
        };
        Ok(Some(BTreeMap::from([
            ("pg_connections".into(), *connections),
            ("pg_wal_bytes".into(), *wal),
            ("pg_temp_bytes".into(), *temporary),
            ("pg_database_bytes".into(), *database),
        ])))
    }

    fn follow_logs(&self, container_id: &ExactContainerId) -> Result<LogFollower, DockerError> {
        self.spawn_follow_logs(container_id)
    }

    fn wait_postgres_ready(
        &self,
        container_id: &ExactContainerId,
        user: &str,
        database: &str,
        timeout: Duration,
    ) -> Result<(), DockerError> {
        if user.is_empty() || database.is_empty() || timeout.is_zero() {
            return Err(DockerError::InvalidRequest(
                "PostgreSQL readiness identity and timeout are required".into(),
            ));
        }
        let mut follower = self.follow_logs(container_id)?;
        let (sender, receiver) = mpsc::channel();
        let mut readers = Vec::new();
        let streams: [Option<Box<dyn Read + Send>>; 2] = [
            follower
                .take_stdout()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
            follower
                .take_stderr()
                .map(|stream| Box::new(stream) as Box<dyn Read + Send>),
        ];
        for stream in streams.into_iter().flatten() {
            let sender = sender.clone();
            readers.push(thread::spawn(move || scan_postgres_ready(stream, sender)));
        }
        drop(sender);
        let deadline = Instant::now() + timeout;
        let mut ready_events = 0_u8;
        let mut open = readers.len();
        let result = loop {
            if ready_events >= 2 {
                let arguments = vec![
                    "pg_isready".into(),
                    "-h".into(),
                    "127.0.0.1".into(),
                    "-U".into(),
                    user.into(),
                    "-d".into(),
                    database.into(),
                ];
                break if self.exec_ok(container_id, &arguments, Duration::from_secs(30)) {
                    Ok(())
                } else {
                    Err(DockerError::Command(
                        "ephemeral PostgreSQL final readiness verification failed".into(),
                    ))
                };
            }
            if open == 0 {
                break Err(DockerError::Command(
                    "ephemeral PostgreSQL logs ended before readiness".into(),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                break Err(DockerError::Timeout {
                    operation: "ephemeral PostgreSQL readiness".into(),
                });
            }
            match receiver.recv_timeout(READINESS_POLL.min(deadline - now)) {
                Ok(PostgresLogEvent::Ready) => ready_events += 1,
                Ok(PostgresLogEvent::Closed) => open = open.saturating_sub(1),
                Ok(PostgresLogEvent::Failed) => {
                    break Err(DockerError::Command(
                        "ephemeral PostgreSQL readiness log failed".into(),
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => open = 0,
            }
        };
        let _ = follower.terminate();
        let cleanup_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match follower.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < cleanup_deadline => thread::sleep(READINESS_POLL),
                _ => {
                    let _ = follower.kill();
                    let _ = follower.wait();
                    break;
                }
            }
        }
        for reader in readers {
            let _ = reader.join();
        }
        result
    }

    fn start_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
        self.require_success(
            vec!["start".into(), container_id.as_str().into()],
            Duration::from_secs(60),
            1_024,
            "docker start failed",
        )
    }

    fn restart_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
        self.require_success(
            vec![
                "restart".into(),
                "--time".into(),
                "15".into(),
                container_id.as_str().into(),
            ],
            Duration::from_secs(90),
            1_024,
            "docker restart failed",
        )
    }

    fn stop_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
        let output = self.invoke(DockerInvocation::new(
            vec![
                "stop".into(),
                "--time".into(),
                "15".into(),
                container_id.as_str().into(),
            ],
            Duration::from_secs(60),
        )?)?;
        if output.success() || output.stderr.contains("No such container") {
            Ok(())
        } else {
            Err(first_error(&output, 1_024, "docker stop failed"))
        }
    }

    fn remove_container(
        &self,
        container_id: &ExactContainerId,
        delete_volumes: bool,
    ) -> Result<(), DockerError> {
        let mut argv = vec!["rm".into(), "--force".into()];
        if delete_volumes {
            argv.push("--volumes".into());
        }
        argv.push(container_id.as_str().into());
        let output = self.invoke(DockerInvocation::new(argv, Duration::from_secs(60))?)?;
        if output.success() || output.stderr.contains("No such container") {
            Ok(())
        } else {
            Err(first_error(&output, 512, "docker rm failed"))
        }
    }

    fn remove_volume(&self, volume: &ManagedVolumeName) -> Result<(), DockerError> {
        let output = self.invoke(DockerInvocation::new(
            vec!["volume".into(), "rm".into(), volume.as_str().into()],
            Duration::from_secs(60),
        )?)?;
        if output.success()
            || output
                .stderr
                .to_ascii_lowercase()
                .contains("no such volume")
        {
            Ok(())
        } else {
            Err(first_error(&output, 512, "docker volume rm failed"))
        }
    }

    fn list_ids_by_labels(
        &self,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<ExactContainerId>, DockerError> {
        let mut argv = vec![
            "ps".into(),
            "--all".into(),
            "--no-trunc".into(),
            "--quiet".into(),
        ];
        for (key, value) in labels {
            argv.push("--filter".into());
            argv.push(format!("label={key}={value}").into());
        }
        let output = self.invoke(DockerInvocation::new(argv, Duration::from_secs(30))?)?;
        if !output.success() {
            return Err(first_error(&output, 512, "docker ps failed"));
        }
        ensure_exact_output(&output, "ps")?;
        output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| ExactContainerId::parse(line.trim().to_owned()))
            .collect()
    }

    fn container_state(&self, container_id: &ExactContainerId) -> ContainerState {
        let Ok(info) = self.inspect(container_id) else {
            return ContainerState::missing();
        };
        container_state_from_inspect(&info)
    }

    fn container_logs(
        &self,
        container_id: &ExactContainerId,
        tail_lines: u16,
    ) -> Result<String, DockerError> {
        if tail_lines == 0 || tail_lines > 5_000 {
            return Err(DockerError::InvalidRequest(
                "tail_lines must be in 1..5000".to_owned(),
            ));
        }
        let output = self.invoke(DockerInvocation::new(
            vec![
                "logs".into(),
                "--tail".into(),
                tail_lines.to_string().into(),
                container_id.as_str().into(),
            ],
            Duration::from_secs(30),
        )?)?;
        Ok(bounded_tail(
            &format!("{}{}", output.stdout, output.stderr),
            MAX_LOG_BYTES,
        ))
    }

    fn compose_invoke(
        &self,
        context: &ComposeContext,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<DockerOutput, DockerError> {
        self.compose_invoke_logged(context, arguments, timeout, None)
    }

    fn compose_invoke_logged(
        &self,
        context: &ComposeContext,
        arguments: &[String],
        timeout: Duration,
        output_log: Option<Arc<Mutex<File>>>,
    ) -> Result<DockerOutput, DockerError> {
        self.compose_invoke_cancellable(context, arguments, timeout, output_log, None)
    }

    fn compose_invoke_cancellable(
        &self,
        context: &ComposeContext,
        arguments: &[String],
        timeout: Duration,
        output_log: Option<Arc<Mutex<File>>>,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<DockerOutput, DockerError> {
        context.validate()?;
        let mut argv = vec![
            "compose".into(),
            "--project-name".into(),
            context.project.clone().into(),
        ];
        for file in &context.files {
            argv.push("--file".into());
            argv.push(file.as_os_str().to_owned());
        }
        for file in &context.env_files {
            argv.push("--env-file".into());
            argv.push(file.as_os_str().to_owned());
        }
        argv.extend(arguments.iter().map(OsString::from));
        self.invoke(
            DockerInvocation::new(argv, timeout)?
                .with_cancellation(cancellation)
                .with_cwd(context.cwd.clone())
                .with_output_log(output_log),
        )
    }

    fn compose_config_services(
        &self,
        context: &ComposeContext,
    ) -> Result<Vec<String>, DockerError> {
        let output = self.compose_invoke(
            context,
            &["config".into(), "--services".into()],
            Duration::from_secs(120),
        )?;
        if !output.success() {
            return Err(first_error(
                &output,
                1_024,
                "compose configuration could not list services",
            ));
        }
        ensure_exact_output(&output, "compose config")?;
        let services = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if services
            .iter()
            .any(|service| !valid_compose_service(service))
        {
            return Err(DockerError::InvalidOutput(
                "compose configuration returned an invalid service name".into(),
            ));
        }
        Ok(services)
    }

    fn compose_up(
        &self,
        context: &ComposeContext,
        services: &[String],
        finite_services: &[String],
        build: bool,
        output_log: Option<Arc<Mutex<File>>>,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<(), DockerError> {
        validate_services(services)?;
        validate_services(finite_services)?;
        let actual = self
            .compose_config_services(context)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let missing = services
            .iter()
            .filter(|service| !actual.contains(*service))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(DockerError::InvalidRequest(format!(
                "compose services not found: {missing:?}"
            )));
        }
        if !finite_services.is_empty() {
            let mut reset = vec!["rm".into(), "--stop".into(), "--force".into()];
            reset.extend(finite_services.iter().cloned());
            let output = self.compose_invoke_cancellable(
                context,
                &reset,
                Duration::from_secs(120),
                None,
                cancellation.clone(),
            )?;
            if !output.success() {
                return Err(first_error(
                    &output,
                    1_024,
                    "compose finite-service reset failed",
                ));
            }
        }
        let mut up = vec!["up".into(), "--detach".into(), "--remove-orphans".into()];
        if build {
            up.push("--build".into());
        }
        up.extend(services.iter().cloned());
        let output = self.compose_invoke_cancellable(
            context,
            &up,
            Duration::from_secs(1_800),
            output_log,
            cancellation,
        )?;
        if output.success() {
            Ok(())
        } else {
            Err(combined_tail_error(&output, "compose up failed"))
        }
    }

    fn compose_stop(&self, context: &ComposeContext) -> Result<(), DockerError> {
        self.compose_require_success(context, &["stop".into()], "compose stop failed")
    }

    fn compose_start(
        &self,
        context: &ComposeContext,
        services: &[String],
    ) -> Result<(), DockerError> {
        validate_services(services)?;
        let mut arguments = vec!["start".into()];
        arguments.extend(services.iter().cloned());
        self.compose_require_success(context, &arguments, "compose start failed")
    }

    fn compose_container_ids(&self, project: &str) -> Result<Vec<ExactContainerId>, DockerError> {
        validate_compose_project(project)?;
        self.list_ids_by_labels(&BTreeMap::from([(
            "com.docker.compose.project".into(),
            project.into(),
        )]))
    }

    fn compose_service_container_ids(
        &self,
        project: &str,
        services: &[String],
    ) -> Result<BTreeMap<String, Vec<ExactContainerId>>, DockerError> {
        validate_services(services)?;
        let wanted = services.iter().cloned().collect::<BTreeSet<_>>();
        let mut found = services
            .iter()
            .cloned()
            .map(|service| (service, Vec::new()))
            .collect::<BTreeMap<_, _>>();
        for container_id in self.compose_container_ids(project)? {
            let state = self.container_state(&container_id);
            if let Some(service) = state.compose_service
                && wanted.contains(&service)
            {
                found.entry(service).or_default().push(container_id);
            }
        }
        let missing = found
            .iter()
            .filter(|(_, ids)| ids.is_empty())
            .map(|(service, _)| service.clone())
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(found)
        } else {
            Err(DockerError::Command(format!(
                "compose service containers not found: {missing:?}"
            )))
        }
    }

    fn compose_start_exact_services(
        &self,
        project: &str,
        services: &[String],
    ) -> Result<(), DockerError> {
        let found = self.compose_service_container_ids(project, services)?;
        for service in services {
            for container_id in &found[service] {
                self.start_container(container_id)?;
            }
        }
        Ok(())
    }

    fn compose_stop_exact_services(
        &self,
        project: &str,
        services: &[String],
    ) -> Result<(), DockerError> {
        let found = self.compose_service_container_ids(project, services)?;
        for service in services {
            for container_id in &found[service] {
                self.stop_container(container_id)?;
            }
        }
        Ok(())
    }

    fn compose_service_ready(
        &self,
        project: &str,
        service: &str,
        timeout: Duration,
    ) -> Result<(bool, String), DockerError> {
        validate_services(&[service.to_owned()])?;
        let deadline = Instant::now() + timeout;
        loop {
            let found = match self.compose_service_container_ids(project, &[service.to_owned()]) {
                Ok(found) => found,
                Err(error) => return Ok((false, error.to_string())),
            };
            let states = found[service]
                .iter()
                .map(|container| self.container_state(container))
                .collect::<Vec<_>>();
            if states
                .iter()
                .all(|state| state.state == RuntimeState::Running)
            {
                return Ok((true, format!("{service} running")));
            }
            if states.iter().any(|state| {
                matches!(
                    state.state,
                    RuntimeState::Failed | RuntimeState::Stopped | RuntimeState::Missing
                )
            }) {
                let detail = states
                    .iter()
                    .map(|state| state.status.as_deref().unwrap_or(state.state.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok((false, format!("{service} became terminal: {detail}")));
            }
            if Instant::now() >= deadline {
                let detail = states
                    .iter()
                    .map(|state| state.state.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok((false, format!("{service} did not become ready: {detail}")));
            }
            thread::sleep(READINESS_POLL);
        }
    }

    fn compose_down(
        &self,
        context: &ComposeContext,
        delete_volumes: bool,
    ) -> Result<(), DockerError> {
        let mut arguments = vec!["down".into(), "--remove-orphans".into()];
        if delete_volumes {
            arguments.push("--volumes".into());
        }
        self.compose_require_success(context, &arguments, "compose down failed")
    }

    fn compose_publishes_host_port(
        &self,
        project: &str,
        host_port: u16,
    ) -> Result<(bool, String), DockerError> {
        for container_id in self.compose_container_ids(project)? {
            let info = match self.inspect(&container_id) {
                Ok(info) => info,
                Err(_) => {
                    return Ok((
                        false,
                        format!("allocated host port {host_port} could not be verified"),
                    ));
                }
            };
            if inspect_publishes_port(&info, host_port) {
                return Ok((
                    true,
                    format!("allocated host port {host_port} is published"),
                ));
            }
        }
        Ok((
            false,
            format!("allocated host port {host_port} is not published by the Compose project"),
        ))
    }

    fn compose_state(
        &self,
        project: &str,
        services: &[String],
        finite_services: &[String],
        completions: &BTreeSet<String>,
        desired_states: &BTreeMap<String, RuntimeState>,
    ) -> Result<ComposeState, DockerError> {
        compose_state(
            self,
            project,
            services,
            finite_services,
            completions,
            desired_states,
        )
    }

    // Keep the existing explicit readiness inputs together with its owned
    // cancellation signal; callers must not infer any of these identities.
    #[allow(clippy::too_many_arguments)]
    fn compose_ready(
        &self,
        project: &str,
        services: &[String],
        finite_services: &[String],
        completions: &BTreeSet<String>,
        desired_states: &BTreeMap<String, RuntimeState>,
        timeout: Duration,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> Result<(bool, String, ComposeState), DockerError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.compose_state(
            project,
            services,
            finite_services,
            completions,
            desired_states,
        )?;
        // An ordinary failed finite service does not invalidate independent
        // calculations that are still running in this same Compose project.
        while (state.state == RuntimeState::Starting
            || state
                .services
                .iter()
                .any(|service| service.role == "finite" && service.state == RuntimeState::Starting)
                && state
                    .services
                    .iter()
                    .filter(|service| service.role != "finite")
                    .all(|service| {
                        !matches!(service.state, RuntimeState::Failed | RuntimeState::Missing)
                    }))
            && Instant::now() < deadline
        {
            if cancellation
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(DockerError::Cancelled {
                    operation: "finite workload readiness".into(),
                });
            }
            thread::sleep(READINESS_POLL);
            state = self.compose_state(
                project,
                services,
                finite_services,
                completions,
                desired_states,
            )?;
        }
        let detail = if state.services.is_empty() {
            state.state.as_str().to_owned()
        } else {
            state
                .services
                .iter()
                .map(|service| format!("{}={}", service.name, service.state.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Ok((
            matches!(state.state, RuntimeState::Running | RuntimeState::Completed),
            detail,
            state,
        ))
    }

    fn compose_logs(
        &self,
        context: &ComposeContext,
        tail_lines: u16,
    ) -> Result<String, DockerError> {
        if tail_lines == 0 || tail_lines > 5_000 {
            return Err(DockerError::InvalidRequest(
                "tail_lines must be in 1..5000".into(),
            ));
        }
        let output = self.compose_invoke(
            context,
            &[
                "logs".into(),
                "--no-color".into(),
                "--tail".into(),
                tail_lines.to_string(),
            ],
            Duration::from_secs(60),
        )?;
        Ok(bounded_tail(
            &format!("{}{}", output.stdout, output.stderr),
            MAX_LOG_BYTES,
        ))
    }

    fn require_success(
        &self,
        argv: Vec<OsString>,
        timeout: Duration,
        error_limit: usize,
        fallback: &str,
    ) -> Result<(), DockerError> {
        let output = self.invoke(DockerInvocation::new(argv, timeout)?)?;
        if output.success() {
            Ok(())
        } else {
            Err(first_error(&output, error_limit, fallback))
        }
    }

    fn compose_require_success(
        &self,
        context: &ComposeContext,
        arguments: &[String],
        fallback: &str,
    ) -> Result<(), DockerError> {
        let output = self.compose_invoke(context, arguments, Duration::from_secs(600))?;
        if output.success() {
            Ok(())
        } else {
            Err(first_error(&output, 1_024, fallback))
        }
    }
}

impl DockerControl for DockerCli {
    fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
        execute_process(&self.executable, invocation)
    }

    fn spawn_follow_logs(
        &self,
        container_id: &ExactContainerId,
    ) -> Result<LogFollower, DockerError> {
        spawn_log_process(&self.executable, container_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeState {
    Running,
    Starting,
    Stopping,
    Stopped,
    Failed,
    Missing,
    Completed,
}

impl RuntimeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Missing => "missing",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerState {
    pub state: RuntimeState,
    pub status: Option<String>,
    pub restarts: u32,
    pub exit_code: Option<i32>,
    pub health: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub image_id: Option<String>,
    pub compose_service: Option<String>,
}

impl ContainerState {
    fn missing() -> Self {
        Self {
            state: RuntimeState::Missing,
            status: None,
            restarts: 0,
            exit_code: None,
            health: None,
            started_at: None,
            finished_at: None,
            image_id: None,
            compose_service: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeContext {
    pub project: String,
    pub files: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub env_files: Vec<PathBuf>,
}

impl ComposeContext {
    pub fn validate(&self) -> Result<(), DockerError> {
        validate_compose_project(&self.project)?;
        if !self.cwd.is_absolute()
            || self.files.is_empty()
            || self.files.iter().any(|path| !path.is_absolute())
            || self.env_files.iter().any(|path| !path.is_absolute())
        {
            return Err(DockerError::InvalidRequest(
                "Compose cwd, files, and environment files must be explicit absolute paths".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeServiceState {
    pub name: String,
    pub role: String,
    pub state: RuntimeState,
    pub desired_state: RuntimeState,
    pub containers: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletionCandidate {
    pub service: String,
    pub container_id: ExactContainerId,
    pub state: ContainerState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComposeState {
    pub state: RuntimeState,
    pub containers: u32,
    pub running: u32,
    pub services: Vec<ComposeServiceState>,
    pub completion_candidates: Vec<CompletionCandidate>,
}

fn image_has_digest<D: DockerControl + ?Sized>(
    docker: &D,
    image: &str,
    requested: &str,
) -> Result<bool, DockerError> {
    let output = docker.invoke(DockerInvocation::new(
        vec![
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{json .RepoDigests}}".into(),
            image.into(),
        ],
        Duration::from_secs(30),
    )?)?;
    if !output.success() {
        return Ok(false);
    }
    ensure_exact_output(&output, "image inspect")?;
    let values: Vec<String> = match serde_json::from_str(output.stdout.trim()) {
        Ok(values) => values,
        Err(_) => return Ok(false),
    };
    Ok(values
        .iter()
        .filter_map(|value| value.rsplit_once('@').map(|(_, digest)| digest))
        .any(|digest| digest == requested))
}

fn container_state_from_inspect(info: &Value) -> ContainerState {
    let state = info.get("State").unwrap_or(&Value::Null);
    let status = state
        .get("Status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let exit_code = state
        .get("ExitCode")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let health = state
        .get("Health")
        .and_then(|value| value.get("Status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut runtime = match status {
        "running" => RuntimeState::Running,
        "created" | "exited" | "paused" => RuntimeState::Stopped,
        "restarting" => RuntimeState::Starting,
        "dead" => RuntimeState::Failed,
        _ => RuntimeState::Failed,
    };
    if status == "exited" && exit_code.is_some_and(|code| code != 0) {
        runtime = RuntimeState::Failed;
    }
    if status == "running" && health.as_deref() == Some("starting") {
        runtime = RuntimeState::Starting;
    } else if status == "running" && health.as_deref() == Some("unhealthy") {
        runtime = RuntimeState::Failed;
    }
    ContainerState {
        state: runtime,
        status: Some(status.to_owned()),
        restarts: info
            .get("RestartCount")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0),
        exit_code,
        health,
        started_at: state
            .get("StartedAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        finished_at: state
            .get("FinishedAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        image_id: info.get("Image").and_then(Value::as_str).map(str::to_owned),
        compose_service: info
            .get("Config")
            .and_then(|value| value.get("Labels"))
            .and_then(|value| value.get("com.docker.compose.service"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn inspect_publishes_port(info: &Value, host_port: u16) -> bool {
    info.get("NetworkSettings")
        .and_then(|value| value.get("Ports"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|ports| ports.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|binding| binding.get("HostPort").and_then(Value::as_str))
        .filter_map(|port| port.parse::<u16>().ok())
        .any(|port| port == host_port)
}

fn validate_compose_project(project: &str) -> Result<(), DockerError> {
    if project.is_empty()
        || project.len() > 128
        || !project
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(DockerError::InvalidRequest(
            "Compose project name is invalid".into(),
        ))
    } else {
        Ok(())
    }
}

fn valid_compose_service(service: &str) -> bool {
    !service.is_empty()
        && service.len() <= 128
        && service
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_services(services: &[String]) -> Result<(), DockerError> {
    if services
        .iter()
        .any(|service| !valid_compose_service(service))
        || services.iter().collect::<BTreeSet<_>>().len() != services.len()
    {
        Err(DockerError::InvalidRequest(
            "Compose service selection is invalid".into(),
        ))
    } else {
        Ok(())
    }
}

fn compose_state<D: DockerControl + ?Sized>(
    docker: &D,
    project: &str,
    services: &[String],
    finite_services: &[String],
    completions: &BTreeSet<String>,
    desired_states: &BTreeMap<String, RuntimeState>,
) -> Result<ComposeState, DockerError> {
    validate_services(services)?;
    validate_services(finite_services)?;
    let ids = docker.compose_container_ids(project)?;
    if ids.is_empty() && completions.is_empty() {
        return Ok(ComposeState {
            state: RuntimeState::Stopped,
            containers: 0,
            running: 0,
            services: Vec::new(),
            completion_candidates: Vec::new(),
        });
    }
    let mut by_service = BTreeMap::<String, Vec<(ExactContainerId, ContainerState)>>::new();
    for container_id in &ids {
        let state = docker.container_state(container_id);
        if let Some(service) = state.compose_service.clone() {
            by_service
                .entry(service)
                .or_default()
                .push((container_id.clone(), state));
        }
    }
    let expected = if services.is_empty() {
        by_service.keys().cloned().collect::<Vec<_>>()
    } else {
        services.to_vec()
    };
    let finite = finite_services.iter().collect::<BTreeSet<_>>();
    let mut details = Vec::new();
    let mut candidates = Vec::new();
    for service in expected {
        let items = by_service.get(&service).cloned().unwrap_or_default();
        let desired = desired_states
            .get(&service)
            .copied()
            .unwrap_or(RuntimeState::Running);
        let state = if finite.contains(&service) {
            let failed = items
                .iter()
                .any(|(_, item)| item.state == RuntimeState::Failed);
            let completed = items.iter().filter(|(_, item)| {
                item.status.as_deref() == Some("exited") && item.exit_code == Some(0)
            });
            let completed = completed.cloned().collect::<Vec<_>>();
            if failed {
                RuntimeState::Failed
            } else if !items.is_empty() && completed.len() == items.len() {
                for (container_id, state) in completed {
                    candidates.push(CompletionCandidate {
                        service: service.clone(),
                        container_id,
                        state,
                    });
                }
                RuntimeState::Completed
            } else if items.iter().any(|(_, item)| {
                matches!(item.state, RuntimeState::Running | RuntimeState::Starting)
            }) {
                RuntimeState::Starting
            } else if completions.contains(&service) {
                RuntimeState::Completed
            } else {
                RuntimeState::Missing
            }
        } else if items.is_empty() {
            RuntimeState::Missing
        } else if desired == RuntimeState::Stopped
            && items
                .iter()
                .all(|(_, item)| matches!(item.state, RuntimeState::Failed | RuntimeState::Stopped))
        {
            RuntimeState::Stopped
        } else if items
            .iter()
            .all(|(_, item)| item.state == RuntimeState::Running)
        {
            RuntimeState::Running
        } else if items
            .iter()
            .any(|(_, item)| item.state == RuntimeState::Failed)
        {
            RuntimeState::Failed
        } else if items
            .iter()
            .any(|(_, item)| item.state == RuntimeState::Starting)
        {
            RuntimeState::Starting
        } else {
            RuntimeState::Failed
        };
        let role = if finite_services.contains(&service) {
            "finite"
        } else {
            "running"
        };
        details.push(ComposeServiceState {
            name: service,
            role: role.into(),
            state,
            desired_state: desired,
            containers: u32::try_from(items.len()).unwrap_or(u32::MAX),
        });
    }
    let all_states = details
        .iter()
        .map(|detail| detail.state)
        .collect::<Vec<_>>();
    let state = if all_states
        .iter()
        .any(|state| matches!(state, RuntimeState::Failed | RuntimeState::Missing))
    {
        RuntimeState::Failed
    } else if all_states.contains(&RuntimeState::Starting) {
        RuntimeState::Starting
    } else {
        let running = details
            .iter()
            .filter(|detail| detail.role == "running")
            .collect::<Vec<_>>();
        let finite = details
            .iter()
            .filter(|detail| detail.role == "finite")
            .collect::<Vec<_>>();
        if running.is_empty()
            && !finite.is_empty()
            && finite
                .iter()
                .all(|detail| detail.state == RuntimeState::Completed)
        {
            RuntimeState::Completed
        } else if !running.is_empty()
            && running
                .iter()
                .all(|detail| detail.state == RuntimeState::Running)
            && finite
                .iter()
                .all(|detail| detail.state == RuntimeState::Completed)
        {
            RuntimeState::Running
        } else if !running.is_empty()
            && running
                .iter()
                .all(|detail| detail.state == RuntimeState::Stopped)
            && finite
                .iter()
                .all(|detail| detail.state == RuntimeState::Completed)
        {
            RuntimeState::Stopped
        } else {
            RuntimeState::Failed
        }
    };
    Ok(ComposeState {
        state,
        containers: u32::try_from(ids.len()).unwrap_or(u32::MAX),
        running: u32::try_from(
            by_service
                .values()
                .flatten()
                .filter(|(_, state)| state.state == RuntimeState::Running)
                .count(),
        )
        .unwrap_or(u32::MAX),
        services: details,
        completion_candidates: candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[derive(Clone, Debug)]
    struct InvocationSnapshot {
        args: Vec<String>,
        cwd: Option<PathBuf>,
        environment: BTreeMap<String, String>,
    }

    struct FakeDocker {
        outputs: Mutex<VecDeque<DockerOutput>>,
        calls: Mutex<Vec<InvocationSnapshot>>,
    }

    impl FakeDocker {
        fn new(outputs: Vec<DockerOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<InvocationSnapshot> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl DockerControl for FakeDocker {
        fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            self.calls.lock().expect("calls").push(InvocationSnapshot {
                args: invocation
                    .args
                    .iter()
                    .map(|value| value.to_string_lossy().into_owned())
                    .collect(),
                cwd: invocation.cwd,
                environment: invocation
                    .environment
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
            });
            self.outputs
                .lock()
                .expect("outputs")
                .pop_front()
                .ok_or_else(|| DockerError::InvalidOutput("unexpected fake Docker call".into()))
        }

        fn spawn_follow_logs(
            &self,
            _container_id: &ExactContainerId,
        ) -> Result<LogFollower, DockerError> {
            Err(DockerError::InvalidRequest(
                "fake log following is unavailable".into(),
            ))
        }
    }

    fn output(
        exit_code: i32,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
    ) -> DockerOutput {
        DockerOutput {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn id(byte: char) -> ExactContainerId {
        ExactContainerId::parse(byte.to_string().repeat(64)).expect("id")
    }

    #[test]
    fn private_output_log_preserves_complete_stream_beyond_the_capture_cap() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private.log");
        let log = Arc::new(Mutex::new(File::create(&path).unwrap()));
        let text = format!(
            "first diagnostic\n{}\nlast diagnostic",
            "progress\n".repeat(MAX_CAPTURE_BYTES)
        );
        let captured = capture_stream(
            std::io::Cursor::new(text.as_bytes()),
            Some(Arc::clone(&log)),
        )
        .unwrap();
        assert!(captured.truncated);
        assert_eq!(captured.bytes.len(), MAX_CAPTURE_BYTES);
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[test]
    fn process_output_log_keeps_stdout_and_stderr_without_exposing_it_in_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("private.log");
        let log = Arc::new(Mutex::new(File::create(&path).unwrap()));
        let invocation = DockerInvocation::new(
            vec![
                "-c".into(),
                "printf 'fixture stdout\\n'; printf 'fixture stderr\\n' >&2; exit 23".into(),
            ],
            Duration::from_secs(5),
        )
        .unwrap()
        .with_output_log(Some(log));
        let output = execute_process(Path::new("/bin/sh"), invocation).unwrap();
        assert_eq!(output.exit_code, 23);
        let retained = std::fs::read_to_string(path).unwrap();
        assert!(retained.contains("fixture stdout"));
        assert!(retained.contains("fixture stderr"));
    }

    #[test]
    fn postgres_readiness_scanner_counts_exact_events_without_retaining_logs() {
        let (sender, receiver) = mpsc::channel();
        scan_postgres_ready(
            std::io::Cursor::new(
                b"database system is ready to accept connections\nnoise\ndatabase system is ready to accept connections\n"
                    .to_vec(),
            ),
            sender,
        );
        let events = receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PostgresLogEvent::Ready))
                .count(),
            2
        );
        assert!(matches!(events.last(), Some(PostgresLogEvent::Closed)));
    }

    #[test]
    fn postgres_facts_are_numeric_and_use_one_fixed_exact_id_query() {
        let docker = FakeDocker::new(vec![output(0, "1|2|3|4\n", "")]);
        let facts = docker
            .postgres_facts(&id('d'), "app", "app_test")
            .unwrap()
            .unwrap();
        assert_eq!(facts["pg_connections"], 1);
        assert_eq!(facts["pg_database_bytes"], 4);
        let calls = docker.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args[0], "exec");
        assert_eq!(calls[0].args[1], "d".repeat(64));
        assert_eq!(
            &calls[0].args[2..8],
            ["psql", "-U", "app", "-d", "app_test", "-tA"]
        );
        assert!(calls[0].args.last().unwrap().starts_with("select "));
    }

    fn labels() -> ManagedLabelContext {
        ManagedLabelContext {
            instance: "test".into(),
            repository_id: "r1".into(),
            worktree_id: "w1".into(),
            run_id: Some("run".into()),
            deployment_id: Some("d1".into()),
            component: Some("api".into()),
            generation: Some(2),
            ttl_seconds: None,
            purpose: "deployment".into(),
            caller_uid: 1000,
            client: "codex".into(),
            session: Some("session".into()),
            created_at: "2026-09-03T12:00:00Z".into(),
            data_class: "none".into(),
        }
    }

    #[test]
    fn managed_names_labels_and_secret_environment_are_exact() {
        assert_eq!(container_name("d1", "api"), "devcoordinator2-deploy-d1-api");
        assert_eq!(
            managed_volume_name("d1", "db", "data").as_str(),
            "devcoordinator2-d1-db-data"
        );
        assert_eq!(compose_project("d1", "stack"), "dc2-d1-stack");
        assert!(
            managed_labels(
                &labels(),
                &BTreeMap::from([("devcoordinator2.instance".into(), "escape".into())])
            )
            .is_err()
        );

        let fake = FakeDocker::new(vec![output(0, format!("{}\n", "a".repeat(64)), "")]);
        let container = fake
            .run_detached(&RunDetachedRequest {
                name: "fixture".into(),
                image: "postgres:16-alpine".into(),
                label_context: labels(),
                labels: BTreeMap::from([("owner".into(), "test".into())]),
                env_names: vec!["POSTGRES_PASSWORD".into()],
                env_values: BTreeMap::from([("POSTGRES_PASSWORD".into(), "top-secret".into())]),
                publish: vec!["127.0.0.1::5432".into()],
                tmpfs: vec!["/tmp:rw,noexec".into()],
                command: vec!["postgres".into()],
            })
            .expect("container");
        assert_eq!(container, id('a'));
        let call = &fake.calls()[0];
        assert!(call.args.contains(&"--pull".into()));
        assert!(call.args.contains(&"devcoordinator2.instance=test".into()));
        assert!(
            !call
                .args
                .iter()
                .any(|argument| argument.contains("top-secret"))
        );
        assert_eq!(
            call.environment
                .get("POSTGRES_PASSWORD")
                .map(String::as_str),
            Some("top-secret")
        );
    }

    #[test]
    fn digest_images_inspection_lifecycle_and_exact_targets_preserve_contracts() {
        let digest = "1".repeat(64);
        let image = format!("registry/postgres@sha256:{digest}");
        let fake = FakeDocker::new(vec![
            output(1, "", "missing"),
            output(0, "pulled", ""),
            output(0, format!("[\"registry/postgres@sha256:{digest}\"]"), ""),
            output(0, "started", ""),
            output(0, "restarted", ""),
            output(1, "", "No such container"),
            output(1, "", "No such container"),
            output(1, "", "no such volume"),
        ]);
        fake.ensure_digest_image(&image).expect("digest image");
        let container = id('b');
        fake.start_container(&container).expect("start");
        fake.restart_container(&container).expect("restart");
        fake.stop_container(&container).expect("idempotent stop");
        fake.remove_container(&container, true)
            .expect("idempotent rm");
        fake.remove_volume(&managed_volume_name("d1", "db", "data"))
            .expect("idempotent volume");
        assert!(ExactContainerId::parse("short").is_err());
        let calls = fake.calls();
        assert_eq!(calls[1].args[..2], ["pull", "--quiet"]);
        assert_eq!(calls[4].args[..3], ["restart", "--time", "15"]);
        assert_eq!(calls[6].args[..3], ["rm", "--force", "--volumes"]);
    }

    #[test]
    fn inspect_state_ports_logs_and_label_listing_are_typed_and_bounded() {
        let container = id('c');
        let inspect = serde_json::json!({
            "State": {"Status":"running","ExitCode":0,"Health":{"Status":"healthy"},"StartedAt":"start","FinishedAt":"finish"},
            "RestartCount":2,
            "Image":"sha256:image",
            "Config":{"Labels":{"com.docker.compose.service":"api"}},
            "NetworkSettings":{"Ports":{"8080/tcp":[{"HostPort":"20001"}]}}
        });
        let fake = FakeDocker::new(vec![
            output(0, inspect.to_string(), ""),
            output(0, inspect.to_string(), ""),
            output(0, format!("{}\n", container.as_str()), ""),
            output(0, "stdout", "stderr"),
        ]);
        let state = fake.container_state(&container);
        assert_eq!(state.state, RuntimeState::Running);
        assert_eq!(state.restarts, 2);
        assert_eq!(state.compose_service.as_deref(), Some("api"));
        assert_eq!(
            fake.published_host_port(&container, "8080/tcp").unwrap(),
            20001
        );
        assert_eq!(
            fake.list_ids_by_labels(&BTreeMap::from([("owner".into(), "test".into())]))
                .unwrap(),
            vec![container.clone()]
        );
        assert_eq!(fake.container_logs(&container, 20).unwrap(), "stdoutstderr");
    }

    #[test]
    fn create_exec_compose_controls_logs_and_port_proof_use_explicit_argv() {
        let temporary = tempdir().expect("tempdir");
        let container = id('f');
        let inspect = serde_json::json!({
            "State":{"Status":"running","ExitCode":0},
            "Config":{"Labels":{"com.docker.compose.service":"api"}},
            "NetworkSettings":{"Ports":{"8080/tcp":[{"HostPort":"20003"}]}}
        });
        let fake = FakeDocker::new(vec![
            output(0, format!("{}\n", container), ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, "compose-out", "compose-err"),
            output(0, format!("{}\n", container), ""),
            output(0, inspect.to_string(), ""),
        ]);
        let env_file = temporary.path().join("environment");
        let created = fake
            .create_container(&CreateContainerRequest {
                name: "api".into(),
                image: "image:tag".into(),
                label_context: labels(),
                labels: BTreeMap::new(),
                env_file: Some(env_file.clone()),
                publish: vec!["127.0.0.1:20003:8080".into()],
                volumes: vec!["volume:/data".into()],
                command: vec!["serve".into()],
                restart: "on-failure:5".into(),
            })
            .expect("create");
        assert_eq!(created, container);
        assert!(fake.exec_ok(&container, &["true".into()], Duration::from_secs(1)));
        let context = ComposeContext {
            project: "dc2-d1-stack".into(),
            files: vec![temporary.path().join("compose.yml")],
            cwd: temporary.path().to_owned(),
            env_files: vec![temporary.path().join("dev.env")],
        };
        fake.compose_stop(&context).expect("stop");
        fake.compose_start(&context, &["api".into()])
            .expect("start");
        fake.compose_down(&context, true).expect("down");
        assert_eq!(
            fake.compose_logs(&context, 25).unwrap(),
            "compose-outcompose-err"
        );
        assert!(
            fake.compose_publishes_host_port("dc2-d1-stack", 20003)
                .unwrap()
                .0
        );
        let calls = fake.calls();
        assert!(calls[0].args.contains(&"--env-file".into()));
        assert!(calls[0].args.contains(&env_file.display().to_string()));
        assert_eq!(calls[1].args[..2], ["exec", container.as_str()]);
        assert!(calls[4].args.ends_with(&[
            "down".into(),
            "--remove-orphans".into(),
            "--volumes".into()
        ]));
        assert!(calls[5].args.ends_with(&[
            "logs".into(),
            "--no-color".into(),
            "--tail".into(),
            "25".into()
        ]));
    }

    #[test]
    fn all_finite_compose_workload_is_completed_not_running() {
        for (status, exit_code, expected) in [
            ("exited", 0, RuntimeState::Completed),
            ("exited", 1, RuntimeState::Failed),
            ("running", 0, RuntimeState::Starting),
        ] {
            let container = id('d');
            let info = serde_json::json!({
                "State":{"Status":status,"ExitCode":exit_code},
                "Config":{"Labels":{"com.docker.compose.service":"probe"}}
            });
            let fake = FakeDocker::new(vec![
                output(0, format!("{container}\n"), ""),
                output(0, info.to_string(), ""),
            ]);
            let state = fake
                .compose_state(
                    "dc2-finite-probe",
                    &["probe".into()],
                    &["probe".into()],
                    &BTreeSet::new(),
                    &BTreeMap::new(),
                )
                .expect("finite state");
            assert_eq!(state.state, expected);
            assert_ne!(state.state, RuntimeState::Running);
        }
    }

    #[test]
    fn compose_up_and_state_preserve_finite_completion_and_selected_service_control() {
        let temporary = tempdir().expect("tempdir");
        let context = ComposeContext {
            project: "dc2-d1-stack".into(),
            files: vec![temporary.path().join("compose.yml")],
            cwd: temporary.path().to_owned(),
            env_files: vec![temporary.path().join("dev.env")],
        };
        let finite = id('d');
        let api = id('e');
        let finite_info = serde_json::json!({
            "State":{"Status":"exited","ExitCode":0},
            "Config":{"Labels":{"com.docker.compose.service":"bootstrap"}}
        });
        let api_info = serde_json::json!({
            "State":{"Status":"running","ExitCode":0},
            "Config":{"Labels":{"com.docker.compose.service":"api"}},
            "NetworkSettings":{"Ports":{"8080/tcp":[{"HostPort":"20002"}]}}
        });
        let fake = FakeDocker::new(vec![
            output(0, "db\nbootstrap\napi\n", ""),
            output(0, "", ""),
            output(0, "", ""),
            output(0, format!("{}\n{}\n", finite, api), ""),
            output(0, finite_info.to_string(), ""),
            output(0, api_info.to_string(), ""),
            output(0, format!("{}\n{}\n", finite, api), ""),
            output(0, finite_info.to_string(), ""),
            output(0, api_info.to_string(), ""),
            output(0, "started", ""),
        ]);
        fake.compose_up(
            &context,
            &["db".into(), "bootstrap".into(), "api".into()],
            &["bootstrap".into()],
            true,
            None,
            None,
        )
        .expect("compose up");
        let state = fake
            .compose_state(
                "dc2-d1-stack",
                &["bootstrap".into(), "api".into()],
                &["bootstrap".into()],
                &BTreeSet::new(),
                &BTreeMap::new(),
            )
            .expect("state");
        assert_eq!(state.state, RuntimeState::Running);
        assert_eq!(state.completion_candidates.len(), 1);
        fake.compose_start_exact_services("dc2-d1-stack", &["api".into()])
            .expect("exact start");
        let calls = fake.calls();
        assert!(
            calls[0]
                .args
                .ends_with(&["config".into(), "--services".into()])
        );
        assert!(calls[1].args.ends_with(&[
            "rm".into(),
            "--stop".into(),
            "--force".into(),
            "bootstrap".into()
        ]));
        assert!(calls[2].args.contains(&"--build".into()));
        assert_eq!(calls.last().unwrap().args[0], "start");
        assert_eq!(
            calls.last().unwrap().args.last().map(String::as_str),
            Some(api.as_str())
        );
        assert_eq!(calls[0].cwd.as_deref(), Some(temporary.path()));
    }

    #[test]
    fn compose_readiness_collects_finite_siblings_after_ordinary_failure() {
        let failed = id('a');
        let slow = id('b');
        let evidence = id('c');
        let info = |service: &str, status: &str, code: i32| {
            serde_json::json!({
                "State": {"Status": status, "ExitCode": code},
                "Config": {"Labels": {"com.docker.compose.service": service}}
            })
            .to_string()
        };
        let ids = format!("{failed}\n{slow}\n{evidence}\n");
        let fake = FakeDocker::new(vec![
            output(0, ids.clone(), ""),
            output(0, info("failed", "exited", 1), ""),
            output(0, info("slow", "running", 0), ""),
            output(0, info("evidence", "running", 0), ""),
            output(0, ids, ""),
            output(0, info("failed", "exited", 1), ""),
            output(0, info("slow", "exited", 0), ""),
            output(0, info("evidence", "running", 0), ""),
        ]);
        let (ready, _, state) = fake
            .compose_ready(
                "dc2-d1-stack",
                &["failed".into(), "slow".into(), "evidence".into()],
                &["failed".into(), "slow".into()],
                &BTreeSet::new(),
                &BTreeMap::new(),
                Duration::from_secs(2),
                None,
            )
            .expect("settled result");
        assert!(!ready);
        assert_eq!(state.state, RuntimeState::Failed);
        assert_eq!(state.completion_candidates.len(), 1);
        assert_eq!(state.completion_candidates[0].service, "slow");
        assert!(state.services.iter().any(|service| service.name == "evidence" && service.state == RuntimeState::Running));
        assert_eq!(fake.calls().len(), 8);
    }

    #[test]
    fn cancellation_stops_the_owned_command_group_and_preserves_output() {
        let temporary = tempdir().expect("tempdir");
        let executable = temporary.path().join("docker");
        std::fs::write(
            &executable,
            "#!/bin/sh\nsleep 30 &\nprintf 'ready\\n'\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log_path = temporary.path().join("private.log");
        let log = Arc::new(Mutex::new(File::create(&log_path).unwrap()));
        let cancellation = Arc::new(AtomicBool::new(false));
        let invocation = DockerInvocation::new(vec!["version".into()], Duration::from_secs(45))
            .unwrap()
            .with_output_log(Some(log))
            .with_cancellation(Some(cancellation.clone()));
        let handle = thread::spawn(move || DockerCli::new(executable).invoke(invocation));
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::fs::metadata(&log_path).unwrap().len() == 0 {
            assert!(Instant::now() < deadline, "fixture did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let stopped = Instant::now();
        cancellation.store(true, Ordering::Release);
        let error = match handle.join().unwrap() {
            Ok(_) => panic!("cancellation expected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), DockerErrorKind::Cancelled);
        assert!(
            stopped.elapsed() < Duration::from_secs(3),
            "a descendant retained the output pipe"
        );
        assert_eq!(std::fs::read_to_string(log_path).unwrap(), "ready\n");
        let cancelled = DockerInvocation::new(vec!["version".into()], Duration::from_secs(1))
            .unwrap()
            .with_cancellation(Some(cancellation));
        assert!(matches!(
            DockerCli::new("/missing/cancelled-fixture").invoke(cancelled),
            Err(DockerError::Cancelled { .. })
        ));
    }

    #[test]
    fn production_runner_clears_ambient_environment_and_bounds_timeout() {
        let temporary = tempdir().expect("tempdir");
        let executable = temporary.path().join("docker");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'HOME=%s\\nPATH=%s\\n' \"${HOME-unset}\" \"${PATH-unset}\"\nprintf '%s\\n' \"$@\"\n",
        )
        .expect("fake docker");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = DockerCli::new(&executable);
        let result = cli
            .invoke(DockerInvocation::new(vec!["version".into()], Duration::from_secs(2)).unwrap())
            .expect("invoke");
        assert!(result.stdout.contains("HOME=unset"));
        assert!(result.stdout.contains("PATH=/usr/bin:/bin"));

        std::fs::write(&executable, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        let error = match cli.invoke(
            DockerInvocation::new(vec!["version".into()], Duration::from_millis(10)).unwrap(),
        ) {
            Ok(_) => panic!("timeout expected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), DockerErrorKind::TimedOut);
    }
}
