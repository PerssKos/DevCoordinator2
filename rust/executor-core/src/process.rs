use std::collections::BTreeMap;
use std::future::{Future, pending};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use devcoordinator2_executor_protocol::{
    CompletionEvent, CompletionMode, EventStatus, FailureIndexEntry, LeafStatus, LogRef, LogStream,
    LogStreamSummary, MAX_DIAGNOSTIC_EVENTS, MAX_EVENT_BYTES, MAX_MANIFEST_BYTES,
};
use rustix::fd::AsFd;
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::pipe::pipe;
use rustix::process::test_kill_process_group;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use crate::ExecutorError;
use crate::capacity::{AcquiredPermit, PermitProvider, PermitRequest};
use crate::diagnostics::{DiagnosticContext, parse_diagnostic_event};
use crate::log_store::{
    CompleteLogWriter, LeafSelector, LogStoreError, RunLogLease, StreamMetadata,
};

const DIAGNOSTIC_FD: i32 = 197;
const EVENT_FD: i32 = 198;
const MANIFEST_FD: i32 = 199;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const GROUP_OBSERVATION_INTERVAL: Duration = Duration::from_millis(50);
// `pipe2(O_CLOEXEC)` is unavailable on Apple platforms. Serializing the
// pipe/fcntl/spawn window keeps another executor leaf from inheriting a pipe
// before both portable `pipe()` descriptors have been marked close-on-exec.
static SPAWN_DESCRIPTOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone)]
pub struct Cancellation {
    sender: watch::Sender<bool>,
}

impl Default for Cancellation {
    fn default() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }
}

impl Cancellation {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

pub(crate) struct ProcessRequest {
    pub progress: mpsc::UnboundedSender<crate::progress::ProcessUpdate>,
    pub run_id: String,
    pub check_name: String,
    pub leaf_id: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub scratch: PathBuf,
    pub shared_artifacts: PathBuf,
    pub diagnostics_dir: PathBuf,
    pub evidence_dir: PathBuf,
    pub log_lease: Arc<RunLogLease>,
    pub log_selector: LeafSelector,
    pub timeout_seconds: Option<u64>,
    pub completion: CompletionMode,
    pub capture_manifest: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Unsafe,
}

impl From<ProcessStatus> for LeafStatus {
    fn from(value: ProcessStatus) -> Self {
        match value {
            ProcessStatus::Passed => Self::Passed,
            ProcessStatus::Failed => Self::Failed,
            ProcessStatus::TimedOut => Self::TimedOut,
            ProcessStatus::Cancelled => Self::Cancelled,
            ProcessStatus::Unsafe => Self::Unsafe,
        }
    }
}

pub(crate) struct ProcessResult {
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub streams: Vec<LogStreamSummary>,
    pub diagnostics: Vec<FailureIndexEntry>,
    pub log_storage_failed: bool,
    pub structured_evidence_invalid: bool,
    pub process_started: bool,
    pub service: Option<EventService>,
    pub manifest: Option<ManifestCapture>,
    pub capacity: crate::capacity::CapacityObservation,
}

pub(crate) struct ManifestCapture {
    pub payload: Vec<u8>,
    pub observed: u64,
    pub truncated: bool,
}

pub(crate) struct EventService {
    pub pgid: i32,
    pub future: Pin<Box<dyn Future<Output = ServiceExit> + Send>>,
}

pub(crate) struct ServiceExit {
    pub exit_code: Option<i32>,
    pub streams: Vec<LogStreamSummary>,
    pub diagnostics: Vec<FailureIndexEntry>,
    pub log_storage_failed: bool,
    pub structured_evidence_invalid: bool,
}

struct SpawnedProcess {
    child: Child,
    pgid: i32,
    stdout: JoinHandle<Result<StreamMetadata, PumpError>>,
    stderr: JoinHandle<Result<StreamMetadata, PumpError>>,
    diagnostics: JoinHandle<Result<Vec<FailureIndexEntry>, ExecutorError>>,
    fatal: mpsc::UnboundedReceiver<FatalLeafError>,
    _fatal_guard: mpsc::UnboundedSender<FatalLeafError>,
    run_id: String,
    event_reader: Option<File>,
    manifest: Option<JoinHandle<Result<ManifestCapture, ExecutorError>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FatalLeafError {
    LogStorage,
    StructuredEvidence,
}

#[derive(Debug)]
enum PumpError {
    Storage(LogStoreError),
    Read(std::io::Error),
}

struct SpawnFailure {
    error: ExecutorError,
    streams: Vec<StreamMetadata>,
    storage_failed: bool,
}

impl From<ExecutorError> for SpawnFailure {
    fn from(error: ExecutorError) -> Self {
        Self {
            error,
            streams: Vec::new(),
            storage_failed: true,
        }
    }
}

pub(crate) async fn run_process(
    request: ProcessRequest,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
) -> ProcessResult {
    let mut progress = crate::progress::ProgressReporter::new(
        request.progress.clone(),
        request.leaf_id.clone(),
        request.log_selector.clone(),
    );
    let mut result = run_process_observed(request, permits, cancellation, &mut progress).await;
    if let Some(service) = result.service.take() {
        // A readiness event completes its check while the service process and
        // its capacity permit remain owned until the graph releases them.
        result.service = Some(EventService {
            pgid: service.pgid,
            future: Box::pin(async move {
                let exit = service.future.await;
                progress.finish(if exit.exit_code == Some(0) {
                    LeafStatus::Passed
                } else {
                    LeafStatus::Cancelled
                });
                drop(progress);
                exit
            }),
        });
    } else {
        progress.finish(result.status.into());
    }
    result
}

async fn run_process_observed(
    request: ProcessRequest,
    permits: Arc<dyn PermitProvider>,
    cancellation: Cancellation,
    progress: &mut crate::progress::ProgressReporter,
) -> ProcessResult {
    let acquire = permits.acquire(PermitRequest {
        run_id: request.run_id.clone(),
        leaf_id: request.leaf_id.clone(),
    });
    let permit = tokio::select! {
        result = acquire => match result {
            Ok(permit) => permit,
            Err(error) => return failure(ProcessStatus::Unsafe, None, error.to_string()),
        },
        () = cancellation.cancelled() => {
            return failure(ProcessStatus::Cancelled, None, "run cancelled before capacity grant");
        }
    };
    let capacity = permit.observation;
    progress.admitted(capacity);
    if let Err(error) = tokio::fs::create_dir_all(&request.scratch).await {
        return ProcessResult {
            capacity,
            ..failure(
                ProcessStatus::Failed,
                None,
                format!("cannot prepare leaf scratch directory: {error}"),
            )
        };
    }
    let stdout_writer = match request
        .log_lease
        .create_stream(request.log_selector.clone(), LogStream::Stdout)
    {
        Ok(writer) => writer,
        Err(error) => {
            return ProcessResult {
                capacity,
                ..storage_failure(None, error.to_string())
            };
        }
    };
    let stderr_writer = match request
        .log_lease
        .create_stream(request.log_selector.clone(), LogStream::Stderr)
    {
        Ok(writer) => writer,
        Err(error) => {
            drop(stdout_writer);
            return ProcessResult {
                capacity,
                ..storage_failure(None, error.to_string())
            };
        }
    };
    if let Err(error) =
        prepare_private_directory(&request.diagnostics_dir, "diagnostic report").await
    {
        drop(stdout_writer);
        drop(stderr_writer);
        return ProcessResult {
            capacity,
            ..storage_failure(None, error.to_string())
        };
    }
    let spawned = match spawn_process(&request, stdout_writer, stderr_writer).await {
        Ok(spawned) => spawned,
        Err(spawn) => {
            let mut result = if spawn.storage_failed {
                storage_failure(None, format!("cannot start leaf: {}", spawn.error))
            } else {
                failure(
                    ProcessStatus::Failed,
                    None,
                    format!("cannot start leaf: {}", spawn.error),
                )
            };
            result.capacity = capacity;
            result.streams = spawn
                .streams
                .into_iter()
                .map(|metadata| stream_summary(&request.run_id, metadata))
                .collect();
            return result;
        }
    };
    progress.executing();
    let mut result = match request.completion {
        CompletionMode::Process => {
            run_to_exit(
                spawned,
                permit,
                request.timeout_seconds,
                cancellation,
                capacity,
            )
            .await
        }
        CompletionMode::Event => {
            run_to_event(
                spawned,
                permit,
                request.timeout_seconds,
                cancellation,
                &request.run_id,
                &request.check_name,
                capacity,
            )
            .await
        }
    };
    result.process_started = true;
    result
}

async fn prepare_private_directory(path: &PathBuf, label: &str) -> Result<(), ExecutorError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|error| ExecutorError::new(format!("cannot create {label} directory: {error}")))?;
    let diagnostics_metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ExecutorError::new(format!("cannot inspect {label} directory: {error}"))
    })?;
    if !diagnostics_metadata.is_dir() || diagnostics_metadata.file_type().is_symlink() {
        return Err(ExecutorError::new(format!(
            "{label} directory is not a real directory"
        )));
    }
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| {
            ExecutorError::new(format!("cannot protect {label} directory: {error}"))
        })?;
    Ok(())
}

async fn spawn_process(
    request: &ProcessRequest,
    stdout_writer: CompleteLogWriter,
    stderr_writer: CompleteLogWriter,
) -> Result<SpawnedProcess, SpawnFailure> {
    if request.command.is_empty() {
        return Err(ExecutorError::new("leaf command is empty").into());
    }
    let mut command = Command::new(&request.command[0]);
    command
        .args(&request.command[1..])
        .current_dir(&request.cwd)
        .envs(&request.env)
        .env("DEVCOORDINATOR_RUN_ID", &request.run_id)
        .env("DEVCOORDINATOR_CHECK_NAME", &request.check_name)
        .env("DEVCOORDINATOR_CHECK_SCRATCH", &request.scratch)
        .env("DEVCOORDINATOR_SHARED_ARTIFACTS", &request.shared_artifacts)
        .env("DEVCOORDINATOR_DIAGNOSTICS_DIR", &request.diagnostics_dir)
        .env("DEVCOORDINATOR_EVIDENCE_DIR", &request.evidence_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);

    let spawn_guard = SPAWN_DESCRIPTOR_LOCK
        .lock()
        .map_err(|_| ExecutorError::new("process descriptor lock is poisoned"))?;
    let (diagnostic_read, diagnostic_write) = cloexec_pipe("diagnostic")?;
    let diagnostic_write_raw = diagnostic_write.as_raw_fd();
    command.env("DEVCOORDINATOR_DIAGNOSTIC_FD", DIAGNOSTIC_FD.to_string());
    // SAFETY: pre_exec invokes only async-signal-safe descriptor operations.
    unsafe {
        command.pre_exec(move || {
            let source = BorrowedFd::borrow_raw(diagnostic_write_raw);
            let mut target = OwnedFd::from_raw_fd(DIAGNOSTIC_FD);
            if diagnostic_write_raw != DIAGNOSTIC_FD {
                rustix::io::dup2(source, &mut target)?;
            }
            fcntl_setfd(target.as_fd(), FdFlags::empty())?;
            std::mem::forget(target);
            Ok(())
        });
    }
    let mut event_read = None;
    let mut event_write = None;
    if request.completion == CompletionMode::Event {
        let (read, write) = cloexec_pipe("event")?;
        let write_raw = write.as_raw_fd();
        command.env("DEVCOORDINATOR_EVENT_FD", EVENT_FD.to_string());
        // SAFETY: pre_exec invokes only async-signal-safe descriptor operations.
        // The target descriptor is deliberately leaked into the child and is
        // closed by exec or process termination.
        unsafe {
            command.pre_exec(move || {
                let source = BorrowedFd::borrow_raw(write_raw);
                let mut target = OwnedFd::from_raw_fd(EVENT_FD);
                if write_raw != EVENT_FD {
                    rustix::io::dup2(source, &mut target)?;
                }
                fcntl_setfd(target.as_fd(), FdFlags::empty())?;
                std::mem::forget(target);
                Ok(())
            });
        }
        event_read = Some(read);
        event_write = Some(write);
    }
    let mut manifest_read = None;
    let mut manifest_write = None;
    if request.capture_manifest {
        let (read, write) = cloexec_pipe("manifest")?;
        let write_raw = write.as_raw_fd();
        command.env("DEVCOORDINATOR_CASE_MANIFEST_FD", MANIFEST_FD.to_string());
        // SAFETY: see the event descriptor setup above. This is a distinct
        // inherited descriptor and uses only async-signal-safe operations.
        unsafe {
            command.pre_exec(move || {
                let source = BorrowedFd::borrow_raw(write_raw);
                let mut target = OwnedFd::from_raw_fd(MANIFEST_FD);
                if write_raw != MANIFEST_FD {
                    rustix::io::dup2(source, &mut target)?;
                }
                fcntl_setfd(target.as_fd(), FdFlags::empty())?;
                std::mem::forget(target);
                Ok(())
            });
        }
        manifest_read = Some(read);
        manifest_write = Some(write);
    }

    let spawned = command.spawn();
    drop(diagnostic_write);
    drop(event_write);
    drop(manifest_write);
    drop(spawn_guard);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let (streams, storage_failed) = seal_prestart_streams(stdout_writer, stderr_writer);
            return Err(SpawnFailure {
                error: ExecutorError::new(error.to_string()),
                streams,
                storage_failed,
            });
        }
    };
    let pgid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .ok_or_else(|| ExecutorError::new("spawned leaf has no valid process id"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExecutorError::new("spawned leaf has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExecutorError::new("spawned leaf has no stderr pipe"))?;
    let (fatal_tx, fatal) = mpsc::unbounded_channel();
    let stdout_failure = fatal_tx.clone();
    let stdout = tokio::spawn(async move { pump(stdout, stdout_writer, stdout_failure).await });
    let stderr_failure = fatal_tx.clone();
    let stderr = tokio::spawn(async move { pump(stderr, stderr_writer, stderr_failure).await });
    let context = DiagnosticContext {
        run_id: request.run_id.clone(),
        check: request.check_name.clone(),
        case: request.log_selector.case_id.clone(),
        phase: request.log_selector.phase,
    };
    let diagnostic_failure = fatal_tx.clone();
    let diagnostics = tokio::spawn(async move {
        read_diagnostic_events(
            File::from_std(std::fs::File::from(diagnostic_read)),
            context,
            diagnostic_failure,
        )
        .await
    });
    let event_reader = event_read.map(|fd| File::from_std(std::fs::File::from(fd)));
    let manifest = manifest_read.map(|fd| {
        tokio::spawn(async move { capture_manifest(File::from_std(std::fs::File::from(fd))).await })
    });
    Ok(SpawnedProcess {
        child,
        pgid,
        stdout,
        stderr,
        diagnostics,
        fatal,
        _fatal_guard: fatal_tx,
        run_id: request.run_id.clone(),
        event_reader,
        manifest,
    })
}

fn cloexec_pipe(kind: &str) -> Result<(OwnedFd, OwnedFd), ExecutorError> {
    let (read, write) = pipe()
        .map_err(|error| ExecutorError::new(format!("cannot create {kind} pipe: {error}")))?;
    for (end, descriptor) in [("read", &read), ("write", &write)] {
        let mut flags = fcntl_getfd(descriptor.as_fd()).map_err(|error| {
            ExecutorError::new(format!(
                "cannot inspect {kind} pipe {end} descriptor: {error}"
            ))
        })?;
        flags.insert(FdFlags::CLOEXEC);
        fcntl_setfd(descriptor.as_fd(), flags).map_err(|error| {
            ExecutorError::new(format!(
                "cannot protect {kind} pipe {end} descriptor: {error}"
            ))
        })?;
    }
    Ok((read, write))
}

async fn run_to_exit(
    mut process: SpawnedProcess,
    _permit: AcquiredPermit,
    timeout_seconds: Option<u64>,
    cancellation: Cancellation,
    capacity: crate::capacity::CapacityObservation,
) -> ProcessResult {
    let deadline = leaf_deadline(timeout_seconds);
    tokio::pin!(deadline);
    let outcome = tokio::select! {
        status = process.child.wait() => match status {
            Ok(status) => {
                let code = exit_code(status);
                if status.success() {
                    (ProcessStatus::Passed, code, None, None)
                } else {
                    (ProcessStatus::Failed, code, Some(format!("process exited {}", display_exit(status))), None)
                }
            }
            Err(error) => (ProcessStatus::Unsafe, None, Some(format!("cannot wait for leaf: {error}")), None),
        },
        () = &mut deadline => {
            let detail = terminate(&mut process.child, process.pgid).await.err().map(|error| error.to_string());
            (ProcessStatus::TimedOut, process.child.try_wait().ok().flatten().and_then(exit_code),
             Some(detail.unwrap_or_else(|| deadline_reason(timeout_seconds, "leaf"))), None)
        },
        () = cancellation.cancelled() => {
            let detail = terminate(&mut process.child, process.pgid).await.err().map(|error| error.to_string());
            (ProcessStatus::Cancelled, process.child.try_wait().ok().flatten().and_then(exit_code),
             Some(detail.unwrap_or_else(|| "run cancelled".into())), None)
        },
        fatal = process.fatal.recv() => {
            let fatal = fatal.unwrap_or(FatalLeafError::LogStorage);
            let detail = terminate(&mut process.child, process.pgid).await.err().map(|error| error.to_string());
            (ProcessStatus::Unsafe, process.child.try_wait().ok().flatten().and_then(exit_code),
             Some(detail.unwrap_or_else(|| fatal_reason(fatal).into())), Some(fatal))
        },
    };
    let cleanup_error = cleanup_process_group(process.pgid).await.err();
    let output = finish_pumps(process.stdout, process.stderr, &process.run_id).await;
    let diagnostics = finish_diagnostics(process.diagnostics).await;
    let manifest = finish_manifest(process.manifest).await;
    let manifest_invalid = manifest.is_err();
    let manifest = manifest.ok().flatten();
    if cleanup_error.is_none()
        && !output.storage_failed
        && !diagnostics.invalid
        && !manifest_invalid
    {
        ProcessResult {
            status: outcome.0,
            exit_code: outcome.1,
            reason: outcome.2,
            streams: output.streams,
            diagnostics: diagnostics.entries,
            log_storage_failed: outcome.3 == Some(FatalLeafError::LogStorage),
            structured_evidence_invalid: outcome.3 == Some(FatalLeafError::StructuredEvidence),
            process_started: true,
            service: None,
            manifest,
            capacity,
        }
    } else {
        let detail = cleanup_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| {
                if output.storage_failed {
                    "log storage became incomplete".into()
                } else {
                    "structured diagnostic evidence is invalid".into()
                }
            });
        let mut result = failure(ProcessStatus::Unsafe, outcome.1, detail);
        result.capacity = capacity;
        result.streams = output.streams;
        result.diagnostics = diagnostics.entries;
        result.log_storage_failed =
            output.storage_failed || outcome.3 == Some(FatalLeafError::LogStorage);
        result.structured_evidence_invalid = diagnostics.invalid
            || manifest_invalid
            || outcome.3 == Some(FatalLeafError::StructuredEvidence);
        result.manifest = manifest;
        result
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_to_event(
    mut process: SpawnedProcess,
    permit: AcquiredPermit,
    timeout_seconds: Option<u64>,
    cancellation: Cancellation,
    run_id: &str,
    check_name: &str,
    capacity: crate::capacity::CapacityObservation,
) -> ProcessResult {
    let Some(mut event_reader) = process.event_reader.take() else {
        let _ = terminate(&mut process.child, process.pgid).await;
        return finalize_process(
            process,
            ProcessStatus::Unsafe,
            None,
            Some("event leaf has no event descriptor".into()),
            capacity,
            Some(FatalLeafError::StructuredEvidence),
        )
        .await;
    };
    let deadline = leaf_deadline(timeout_seconds);
    tokio::pin!(deadline);
    enum First {
        Event(Result<CompletionEvent, ExecutorError>),
        Exit(Result<std::process::ExitStatus, std::io::Error>),
        Timeout,
        Cancel,
        Fatal(FatalLeafError),
    }
    let first = tokio::select! {
        event = read_event(&mut event_reader) => First::Event(event),
        status = process.child.wait() => First::Exit(status),
        () = &mut deadline => First::Timeout,
        () = cancellation.cancelled() => First::Cancel,
        fatal = process.fatal.recv() => First::Fatal(fatal.unwrap_or(FatalLeafError::LogStorage)),
    };
    match first {
        First::Exit(status) => {
            let (code, reason) = match status {
                Ok(status) => (
                    exit_code(status),
                    format!(
                        "process exited {} before its completion event",
                        display_exit(status)
                    ),
                ),
                Err(error) => (None, format!("cannot wait for event leaf: {error}")),
            };
            finalize_process(
                process,
                ProcessStatus::Failed,
                code,
                Some(reason),
                capacity,
                None,
            )
            .await
        }
        First::Timeout => {
            let _ = terminate(&mut process.child, process.pgid).await;
            let code = process.child.try_wait().ok().flatten().and_then(exit_code);
            finalize_process(
                process,
                ProcessStatus::TimedOut,
                code,
                Some(deadline_reason(timeout_seconds, "event leaf")),
                capacity,
                None,
            )
            .await
        }
        First::Cancel => {
            let _ = terminate(&mut process.child, process.pgid).await;
            let code = process.child.try_wait().ok().flatten().and_then(exit_code);
            finalize_process(
                process,
                ProcessStatus::Cancelled,
                code,
                Some("run cancelled".into()),
                capacity,
                None,
            )
            .await
        }
        First::Fatal(fatal) => {
            let _ = terminate(&mut process.child, process.pgid).await;
            let code = process.child.try_wait().ok().flatten().and_then(exit_code);
            finalize_process(
                process,
                ProcessStatus::Unsafe,
                code,
                Some(fatal_reason(fatal).into()),
                capacity,
                Some(fatal),
            )
            .await
        }
        First::Event(Err(error)) => {
            let _ = terminate(&mut process.child, process.pgid).await;
            let code = process.child.try_wait().ok().flatten().and_then(exit_code);
            finalize_process(
                process,
                ProcessStatus::Unsafe,
                code,
                Some(error.to_string()),
                capacity,
                Some(FatalLeafError::StructuredEvidence),
            )
            .await
        }
        First::Event(Ok(event)) => {
            if event.run_id != run_id || event.check != check_name {
                let _ = terminate(&mut process.child, process.pgid).await;
                let code = process.child.try_wait().ok().flatten().and_then(exit_code);
                return finalize_process(
                    process,
                    ProcessStatus::Unsafe,
                    code,
                    Some("completion event carried the wrong run or check identity".into()),
                    capacity,
                    Some(FatalLeafError::StructuredEvidence),
                )
                .await;
            }
            if event.status != EventStatus::Passed {
                let _ = terminate(&mut process.child, process.pgid).await;
                let code = process.child.try_wait().ok().flatten().and_then(exit_code);
                return finalize_process(
                    process,
                    if event.status == EventStatus::Unsafe {
                        ProcessStatus::Unsafe
                    } else {
                        ProcessStatus::Failed
                    },
                    code,
                    Some(
                        event
                            .reason
                            .unwrap_or_else(|| "completion event reported failure".into()),
                    ),
                    capacity,
                    None,
                )
                .await;
            }
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    finalize_process(
                        process,
                        if status.success() {
                            ProcessStatus::Passed
                        } else {
                            ProcessStatus::Failed
                        },
                        exit_code(status),
                        (!status.success())
                            .then(|| format!("event process exited {}", display_exit(status))),
                        capacity,
                        None,
                    )
                    .await
                }
                Ok(None) => {
                    let pgid = process.pgid;
                    let future = Box::pin(async move {
                        let _permit = permit;
                        let (status, fatal) = tokio::select! {
                            status = process.child.wait() => (status, None),
                            fatal = process.fatal.recv() => {
                                let fatal = fatal.unwrap_or(FatalLeafError::LogStorage);
                                let _ = terminate(&mut process.child, process.pgid).await;
                                (process.child.try_wait().map_err(std::io::Error::other)
                                    .and_then(|status| status.ok_or_else(|| std::io::Error::other("service did not terminate"))), Some(fatal))
                            }
                        };
                        let exit_code = status.ok().and_then(exit_code);
                        let result = finalize_process(
                            process,
                            ProcessStatus::Passed,
                            exit_code,
                            None,
                            Default::default(),
                            fatal,
                        )
                        .await;
                        ServiceExit {
                            exit_code: result.exit_code,
                            streams: result.streams,
                            diagnostics: result.diagnostics,
                            log_storage_failed: result.log_storage_failed,
                            structured_evidence_invalid: result.structured_evidence_invalid,
                        }
                    });
                    ProcessResult {
                        status: ProcessStatus::Passed,
                        exit_code: None,
                        reason: None,
                        streams: Vec::new(),
                        diagnostics: Vec::new(),
                        log_storage_failed: false,
                        structured_evidence_invalid: false,
                        process_started: true,
                        service: Some(EventService { pgid, future }),
                        manifest: None,
                        capacity,
                    }
                }
                Err(error) => {
                    let _ = terminate(&mut process.child, process.pgid).await;
                    finalize_process(
                        process,
                        ProcessStatus::Unsafe,
                        None,
                        Some(format!("cannot inspect event process: {error}")),
                        capacity,
                        None,
                    )
                    .await
                }
            }
        }
    }
}

pub(crate) fn signal_group(pgid: i32, signal: Signal) -> Result<(), ExecutorError> {
    let pid =
        Pid::from_raw(pgid).ok_or_else(|| ExecutorError::new("process group id is invalid"))?;
    match kill_process_group(pid, signal) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(()),
        Err(error) => Err(ExecutorError::new(format!(
            "cannot signal process group {pgid}: {error}"
        ))),
    }
}

async fn terminate(child: &mut Child, pgid: i32) -> Result<(), ExecutorError> {
    if child.try_wait()?.is_some() {
        return cleanup_process_group(pgid).await;
    }
    signal_group(pgid, Signal::TERM)?;
    if timeout(TERMINATION_GRACE, child.wait()).await.is_err() {
        signal_group(pgid, Signal::KILL)?;
        child.wait().await?;
    }
    cleanup_process_group(pgid).await
}

async fn cleanup_process_group(pgid: i32) -> Result<(), ExecutorError> {
    if !process_group_exists(pgid)? {
        return Ok(());
    }
    signal_group(pgid, Signal::TERM)?;
    let deadline = Instant::now() + TERMINATION_GRACE;
    while process_group_exists(pgid)? && Instant::now() < deadline {
        tokio::time::sleep(GROUP_OBSERVATION_INTERVAL).await;
    }
    if process_group_exists(pgid)? {
        signal_group(pgid, Signal::KILL)?;
    }
    Ok(())
}

fn process_group_exists(pgid: i32) -> Result<bool, ExecutorError> {
    let pid =
        Pid::from_raw(pgid).ok_or_else(|| ExecutorError::new("process group id is invalid"))?;
    match test_kill_process_group(pid) {
        Ok(()) => Ok(true),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(false),
        Err(error) => Err(ExecutorError::new(format!(
            "cannot inspect process group {pgid}: {error}"
        ))),
    }
}

async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    mut output: CompleteLogWriter,
    fatal: mpsc::UnboundedSender<FatalLeafError>,
) -> Result<StreamMetadata, PumpError> {
    let mut block = vec![0_u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut block).await {
            Ok(read) => read,
            Err(error) => {
                let _ = fatal.send(FatalLeafError::LogStorage);
                return Err(PumpError::Read(error));
            }
        };
        if read == 0 {
            break;
        }
        if let Err(error) = output.write_all(&block[..read]) {
            let _ = fatal.send(FatalLeafError::LogStorage);
            return Err(PumpError::Storage(error));
        }
    }
    output.seal().map_err(|error| {
        let _ = fatal.send(FatalLeafError::LogStorage);
        PumpError::Storage(error)
    })
}

#[derive(Default)]
struct FinishedPumps {
    streams: Vec<LogStreamSummary>,
    storage_failed: bool,
}

async fn finish_pumps(
    stdout: JoinHandle<Result<StreamMetadata, PumpError>>,
    stderr: JoinHandle<Result<StreamMetadata, PumpError>>,
    run_id: &str,
) -> FinishedPumps {
    let mut result = FinishedPumps::default();
    for task in [stdout, stderr] {
        match task.await {
            Ok(Ok(metadata)) => result.streams.push(stream_summary(run_id, metadata)),
            Ok(Err(PumpError::Storage(error))) => {
                let _ = error.code();
                result.storage_failed = true;
            }
            Ok(Err(PumpError::Read(error))) => {
                let _ = error.kind();
                result.storage_failed = true;
            }
            Err(_) => result.storage_failed = true,
        }
    }
    result
}

fn stream_summary(run_id: &str, metadata: StreamMetadata) -> LogStreamSummary {
    LogStreamSummary {
        log_ref: LogRef {
            run_id: run_id.to_owned(),
            check: metadata.selector.check,
            phase: metadata.selector.phase,
            case: metadata.selector.case_id,
            stream: metadata.stream,
        },
        bytes: metadata.bytes,
        lines: metadata.lines,
        sha256: metadata.sha256,
        first_write_epoch_ms: metadata.first_write_epoch_ms,
        last_write_epoch_ms: metadata.last_write_epoch_ms,
        complete: metadata.complete,
    }
}

fn seal_prestart_streams(
    stdout: CompleteLogWriter,
    stderr: CompleteLogWriter,
) -> (Vec<StreamMetadata>, bool) {
    let mut streams = Vec::new();
    let mut storage_failed = false;
    for writer in [stdout, stderr] {
        match writer.seal() {
            Ok(metadata) => streams.push(metadata),
            Err(_) => storage_failed = true,
        }
    }
    (streams, storage_failed)
}

#[derive(Default)]
struct FinishedDiagnostics {
    entries: Vec<FailureIndexEntry>,
    invalid: bool,
}

async fn finish_diagnostics(
    task: JoinHandle<Result<Vec<FailureIndexEntry>, ExecutorError>>,
) -> FinishedDiagnostics {
    match task.await {
        Ok(Ok(entries)) => FinishedDiagnostics {
            entries,
            invalid: false,
        },
        Ok(Err(_)) | Err(_) => FinishedDiagnostics {
            entries: Vec::new(),
            invalid: true,
        },
    }
}

async fn read_diagnostic_events(
    mut reader: File,
    context: DiagnosticContext,
    fatal: mpsc::UnboundedSender<FatalLeafError>,
) -> Result<Vec<FailureIndexEntry>, ExecutorError> {
    let mut entries = Vec::new();
    let mut current = Vec::new();
    let mut block = [0_u8; 1024];
    loop {
        let read = reader.read(&mut block).await.map_err(|_| {
            let _ = fatal.send(FatalLeafError::StructuredEvidence);
            ExecutorError::new("cannot read structured diagnostic event channel")
        })?;
        if read == 0 {
            break;
        }
        for byte in &block[..read] {
            if *byte == b'\n' {
                if current.is_empty() {
                    continue;
                }
                let entry = parse_diagnostic_event(&current, &context).map_err(|_| {
                    let _ = fatal.send(FatalLeafError::StructuredEvidence);
                    ExecutorError::new("structured diagnostic event is invalid")
                })?;
                entries.push(entry);
                current.clear();
                if entries.len() > MAX_DIAGNOSTIC_EVENTS {
                    let _ = fatal.send(FatalLeafError::StructuredEvidence);
                    return Err(ExecutorError::new(
                        "structured diagnostic event count exceeds its bound",
                    ));
                }
            } else {
                current.push(*byte);
                if current.len() > MAX_EVENT_BYTES {
                    let _ = fatal.send(FatalLeafError::StructuredEvidence);
                    return Err(ExecutorError::new(
                        "structured diagnostic event exceeds 4096 bytes",
                    ));
                }
            }
        }
    }
    if !current.is_empty() {
        let entry = parse_diagnostic_event(&current, &context).map_err(|_| {
            let _ = fatal.send(FatalLeafError::StructuredEvidence);
            ExecutorError::new("structured diagnostic event is invalid")
        })?;
        entries.push(entry);
    }
    if entries.len() > MAX_DIAGNOSTIC_EVENTS {
        let _ = fatal.send(FatalLeafError::StructuredEvidence);
        return Err(ExecutorError::new(
            "structured diagnostic event count exceeds its bound",
        ));
    }
    Ok(entries)
}

async fn finalize_process(
    process: SpawnedProcess,
    status: ProcessStatus,
    exit_code: Option<i32>,
    reason: Option<String>,
    capacity: crate::capacity::CapacityObservation,
    fatal: Option<FatalLeafError>,
) -> ProcessResult {
    let cleanup_failed = cleanup_process_group(process.pgid).await.is_err();
    let output = finish_pumps(process.stdout, process.stderr, &process.run_id).await;
    let diagnostics = finish_diagnostics(process.diagnostics).await;
    let manifest = finish_manifest(process.manifest).await;
    let log_storage_failed = output.storage_failed || fatal == Some(FatalLeafError::LogStorage);
    let structured_evidence_invalid =
        diagnostics.invalid || fatal == Some(FatalLeafError::StructuredEvidence);
    let unsafe_evidence = log_storage_failed || structured_evidence_invalid || cleanup_failed;
    ProcessResult {
        status: if unsafe_evidence {
            ProcessStatus::Unsafe
        } else {
            status
        },
        exit_code,
        reason: if unsafe_evidence {
            Some(
                if log_storage_failed {
                    "log storage became incomplete"
                } else if structured_evidence_invalid {
                    "structured diagnostic evidence is invalid"
                } else {
                    "process group cleanup failed"
                }
                .into(),
            )
        } else {
            reason
        },
        streams: output.streams,
        diagnostics: diagnostics.entries,
        log_storage_failed,
        structured_evidence_invalid,
        process_started: true,
        service: None,
        manifest: manifest.ok().flatten(),
        capacity,
    }
}

const fn fatal_reason(error: FatalLeafError) -> &'static str {
    match error {
        FatalLeafError::LogStorage => "log storage became incomplete",
        FatalLeafError::StructuredEvidence => "structured diagnostic evidence is invalid",
    }
}

async fn capture_manifest(mut reader: File) -> Result<ManifestCapture, ExecutorError> {
    let mut payload = Vec::new();
    let mut observed = 0_u64;
    let mut block = vec![0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut block).await.map_err(|error| {
            ExecutorError::new(format!("cannot read case manifest descriptor: {error}"))
        })?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        let remaining = (MAX_MANIFEST_BYTES + 1).saturating_sub(payload.len());
        if remaining > 0 {
            payload.extend_from_slice(&block[..read.min(remaining)]);
        }
    }
    Ok(ManifestCapture {
        truncated: observed > MAX_MANIFEST_BYTES as u64,
        observed,
        payload,
    })
}

async fn finish_manifest(
    manifest: Option<JoinHandle<Result<ManifestCapture, ExecutorError>>>,
) -> Result<Option<ManifestCapture>, ExecutorError> {
    match manifest {
        Some(task) => task
            .await
            .map_err(|error| ExecutorError::new(format!("manifest task failed: {error}")))?
            .map(Some),
        None => Ok(None),
    }
}

async fn read_event(reader: &mut File) -> Result<CompletionEvent, ExecutorError> {
    let mut payload = Vec::new();
    let mut block = [0_u8; 512];
    loop {
        let read = reader.read(&mut block).await.map_err(|error| {
            ExecutorError::new(format!("cannot read completion event: {error}"))
        })?;
        if read == 0 {
            return Err(ExecutorError::new(
                "completion event descriptor closed without an event",
            ));
        }
        if let Some(newline) = block[..read].iter().position(|byte| *byte == b'\n') {
            if newline + 1 != read {
                return Err(ExecutorError::new(
                    "completion event contains trailing data",
                ));
            }
            payload.extend_from_slice(&block[..newline]);
            break;
        }
        payload.extend_from_slice(&block[..read]);
        if payload.len() > MAX_EVENT_BYTES {
            return Err(ExecutorError::new("completion event exceeds 4096 bytes"));
        }
    }
    if payload.len() > MAX_EVENT_BYTES {
        return Err(ExecutorError::new("completion event exceeds 4096 bytes"));
    }
    serde_json::from_slice(&payload)
        .map_err(|error| ExecutorError::new(format!("invalid completion event: {error}")))
}

fn failure(
    status: ProcessStatus,
    exit_code: Option<i32>,
    reason: impl Into<String>,
) -> ProcessResult {
    ProcessResult {
        status,
        exit_code,
        reason: Some(reason.into()),
        streams: Vec::new(),
        diagnostics: Vec::new(),
        log_storage_failed: false,
        structured_evidence_invalid: false,
        process_started: false,
        service: None,
        manifest: None,
        capacity: Default::default(),
    }
}

fn storage_failure(exit_code: Option<i32>, reason: impl Into<String>) -> ProcessResult {
    let mut result = failure(ProcessStatus::Unsafe, exit_code, reason);
    result.log_storage_failed = true;
    result
}

fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
}

fn display_exit(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .or_else(|| {
            status
                .signal()
                .map(|signal| format!("from signal {signal}"))
        })
        .unwrap_or_else(|| "without a status".into())
}

async fn leaf_deadline(timeout_seconds: Option<u64>) {
    match timeout_seconds {
        Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
        None => pending::<()>().await,
    }
}

fn deadline_reason(timeout_seconds: Option<u64>, kind: &str) -> String {
    match timeout_seconds {
        Some(seconds) => format!("{kind} exceeded {seconds} second deadline"),
        None => format!("{kind} deadline elapsed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_pipe_marks_both_ends_close_on_exec() {
        let (read, write) = cloexec_pipe("test").expect("create protected pipe");
        for descriptor in [&read, &write] {
            let flags = fcntl_getfd(descriptor.as_fd()).expect("read descriptor flags");
            assert!(flags.contains(FdFlags::CLOEXEC));
        }
    }
}
