//! Atomic governed-test admission, upgrade drains, and activity receipts.
//!
//! Runtime files are opened relative to a no-follow directory descriptor. The
//! admission lock is held for the complete start guard, so a drain cannot be
//! published between admission and accepted-activity accounting.

use std::collections::{BTreeMap, HashSet};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard};

use rustix::fs::{
    self as unix_fs, AtFlags, FlockOperation, Mode, OFlags, mkdirat, open, openat, renameat,
    unlinkat,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use time::{OffsetDateTime, format_description::FormatItem, macros::format_description};

pub const LOCK_FILE: &str = "test-admission.lock";
pub const DRAIN_FILE: &str = "test-drain.json";
pub const ACTIVITY_FILE: &str = "test-activity.json";

const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[cfg(target_os = "linux")]
const INOTIFY_EVENTS: u32 =
    libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionErrorKind {
    TestsDraining,
    DrainConflict,
    ProcessIdentityUnavailable,
    ActivityUnavailable,
    InvalidRuntimePath,
    Filesystem,
    Serialization,
    EventWatch,
    StatePoisoned,
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("{reason}")]
    TestsDraining { reason: String },
    #[error("another live test drain already exists")]
    DrainConflict,
    #[error("cannot identify installer process for drain lease")]
    ProcessIdentityUnavailable,
    #[error("test activity receipt is unavailable")]
    ActivityUnavailable,
    #[error("runtime path may not contain parent traversal")]
    InvalidRuntimePath,
    #[error("test admission state lock is poisoned")]
    StatePoisoned,
    #[error("{operation}: {source}")]
    Filesystem {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("serialize test admission document: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("watch test activity mutations: {0}")]
    EventWatch(#[source] io::Error),
}

impl AdmissionError {
    pub const fn kind(&self) -> AdmissionErrorKind {
        match self {
            Self::TestsDraining { .. } => AdmissionErrorKind::TestsDraining,
            Self::DrainConflict => AdmissionErrorKind::DrainConflict,
            Self::ProcessIdentityUnavailable => AdmissionErrorKind::ProcessIdentityUnavailable,
            Self::ActivityUnavailable => AdmissionErrorKind::ActivityUnavailable,
            Self::InvalidRuntimePath => AdmissionErrorKind::InvalidRuntimePath,
            Self::StatePoisoned => AdmissionErrorKind::StatePoisoned,
            Self::Filesystem { .. } => AdmissionErrorKind::Filesystem,
            Self::Serialization(_) => AdmissionErrorKind::Serialization,
            Self::EventWatch(_) => AdmissionErrorKind::EventWatch,
        }
    }

    /// Stable error-code mapping used by the protocol-v2 dispatcher.
    pub const fn protocol_code(&self) -> &'static str {
        match self {
            Self::TestsDraining { .. } => "tests_draining",
            Self::DrainConflict => "test_drain_conflict",
            _ => "test_admission_failed",
        }
    }
}

fn filesystem(operation: &'static str, error: impl Into<io::Error>) -> AdmissionError {
    AdmissionError::Filesystem {
        operation,
        source: error.into(),
    }
}

struct RuntimeDirectory {
    path: PathBuf,
    descriptor: File,
}

impl RuntimeDirectory {
    fn open(path: &Path, create: bool) -> Result<Self, AdmissionError> {
        let anchor = if path.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        };
        let mut directory = open(
            anchor,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| filesystem("open runtime directory anchor", error))?;
        for component in path.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(AdmissionError::InvalidRuntimePath);
                }
            };
            if create {
                match mkdirat(&directory, name, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(filesystem("create runtime directory", error)),
                }
            }
            directory = openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| filesystem("open runtime directory", error))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            descriptor: directory,
        })
    }

    fn open_lock(&self) -> Result<File, AdmissionError> {
        let descriptor = openat(
            &self.descriptor,
            LOCK_FILE,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o666),
        )
        .map_err(|error| filesystem("open test admission lock", error))?;
        let file = File::from(descriptor);
        let metadata = file
            .metadata()
            .map_err(|error| filesystem("inspect test admission lock", error))?;
        if !metadata.is_file() {
            return Err(filesystem(
                "inspect test admission lock",
                io::Error::new(io::ErrorKind::InvalidData, "lock is not a regular file"),
            ));
        }
        unix_fs::fchmod(&file, Mode::from_raw_mode(0o666))
            .map_err(|error| filesystem("set test admission lock mode", error))?;
        Ok(file)
    }

    fn sync(&self, operation: &'static str) -> Result<(), AdmissionError> {
        self.descriptor
            .sync_all()
            .map_err(|error| filesystem(operation, error))
    }
}

struct ExclusiveFileLock<'a> {
    file: &'a File,
}

impl<'a> ExclusiveFileLock<'a> {
    fn acquire(file: &'a File) -> Result<Self, AdmissionError> {
        unix_fs::flock(file, FlockOperation::LockExclusive)
            .map_err(|error| filesystem("lock test admission", error))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveFileLock<'_> {
    fn drop(&mut self) {
        let _ = unix_fs::flock(self.file, FlockOperation::Unlock);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveTest {
    pub run_id: String,
    pub unit: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityReceipt {
    pub schema: u64,
    pub generation: Option<u64>,
    /// The v1 receipt accepted any JSON values inside `active`; retain that
    /// compatibility while exposing `active_tests` for typed consumers.
    pub active: Vec<Value>,
}

impl ActivityReceipt {
    pub fn active_tests(&self) -> Option<Vec<ActiveTest>> {
        self.active
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    }

    pub fn is_zero(&self) -> bool {
        self.active.is_empty()
    }
}

#[derive(Debug, Default)]
struct AdmissionState {
    active: BTreeMap<String, String>,
    generation: u64,
}

pub struct TestAdmission {
    runtime: RuntimeDirectory,
    lock_file: File,
    state: Mutex<AdmissionState>,
    worktree_starts: Mutex<HashSet<String>>,
    worktree_changed: Condvar,
}

fn read_json(runtime: &RuntimeDirectory, name: &str) -> Option<Value> {
    let descriptor = openat(
        &runtime.descriptor,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(descriptor);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_JSON_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.is_object().then_some(value)
}

fn unlink_if_present(
    runtime: &RuntimeDirectory,
    name: &str,
    operation: &'static str,
) -> Result<bool, AdmissionError> {
    match unlinkat(&runtime.descriptor, name, AtFlags::empty()) {
        Ok(()) => {
            runtime.sync("sync removed test admission document")?;
            notify_runtime_mutation(&runtime.path);
            Ok(true)
        }
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(filesystem(operation, error)),
    }
}

fn random_hex(byte_count: usize) -> Result<String, AdmissionError> {
    use std::fmt::Write as _;

    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|error| {
        filesystem(
            "generate test admission nonce",
            io::Error::other(error.to_string()),
        )
    })?;
    let mut encoded = String::with_capacity(byte_count * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

fn atomic_write_json(
    runtime: &RuntimeDirectory,
    name: &str,
    document: &Value,
    mode: u32,
) -> Result<(), AdmissionError> {
    let mut payload = serde_json::to_vec(document)?;
    payload.push(b'\n');
    if payload.len() as u64 > MAX_JSON_BYTES {
        return Err(filesystem(
            "encode test admission document",
            io::Error::new(
                io::ErrorKind::InvalidData,
                "governed-check report exceeds 2 MiB",
            ),
        ));
    }
    let temporary = format!(".{name}-{}", random_hex(16)?);
    let result = (|| -> Result<(), AdmissionError> {
        let descriptor = openat(
            &runtime.descriptor,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| filesystem("create temporary test admission document", error))?;
        let mut file = File::from(descriptor);
        file.write_all(&payload)
            .map_err(|error| filesystem("write temporary test admission document", error))?;
        file.sync_all()
            .map_err(|error| filesystem("sync temporary test admission document", error))?;
        unix_fs::fchmod(&file, Mode::from_raw_mode(mode as _))
            .map_err(|error| filesystem("set test admission document mode", error))?;
        renameat(
            &runtime.descriptor,
            temporary.as_str(),
            &runtime.descriptor,
            name,
        )
        .map_err(|error| filesystem("publish test admission document", error))?;
        runtime.sync("sync published test admission document")?;
        notify_runtime_mutation(&runtime.path);
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(&runtime.descriptor, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn iso_now() -> Result<String, AdmissionError> {
    OffsetDateTime::now_utc()
        .format(TIMESTAMP_FORMAT)
        .map_err(|error| filesystem("format test drain timestamp", io::Error::other(error)))
}

fn process_start(pid: i64) -> Option<String> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let tail = text.get(text.rfind(')')? + 2..)?;
    tail.split_whitespace().nth(19).map(str::to_owned)
}

fn lease_is_live(document: &Value) -> bool {
    let Some(object) = document.as_object() else {
        return false;
    };
    if object.get("schema").and_then(Value::as_u64) != Some(1) {
        return false;
    }
    let Some(pid) = object.get("pid").and_then(Value::as_i64) else {
        return false;
    };
    let Some(expected) = object.get("process_start").and_then(Value::as_str) else {
        return false;
    };
    process_start(pid).as_deref() == Some(expected)
}

fn drain_reason(document: &Value) -> String {
    match document.get("reason") {
        Some(Value::String(reason)) if !reason.is_empty() => reason.clone(),
        Some(Value::Number(reason)) => reason.to_string(),
        Some(Value::Bool(reason)) => {
            if *reason {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        _ => "coordinator upgrade in progress".to_owned(),
    }
}

impl TestAdmission {
    pub fn new(runtime_dir: impl AsRef<Path>) -> Result<Self, AdmissionError> {
        let runtime = RuntimeDirectory::open(runtime_dir.as_ref(), true)?;
        let lock_file = runtime.open_lock()?;
        Ok(Self {
            runtime,
            lock_file,
            state: Mutex::new(AdmissionState::default()),
            worktree_starts: Mutex::new(HashSet::new()),
            worktree_changed: Condvar::new(),
        })
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime.path
    }

    fn state(&self) -> Result<MutexGuard<'_, AdmissionState>, AdmissionError> {
        self.state.lock().map_err(|_| AdmissionError::StatePoisoned)
    }

    fn live_drain(&self) -> Result<Option<Value>, AdmissionError> {
        let Some(document) = read_json(&self.runtime, DRAIN_FILE) else {
            return Ok(None);
        };
        if lease_is_live(&document) {
            return Ok(Some(document));
        }
        unlink_if_present(&self.runtime, DRAIN_FILE, "remove stale test drain lease")?;
        Ok(None)
    }

    fn start_guard_with<'a>(
        &'a self,
        worktree: Option<WorktreeStartPermit<'a>>,
    ) -> Result<StartGuard<'a>, AdmissionError> {
        let state = self.state()?;
        let file_lock = ExclusiveFileLock::acquire(&self.lock_file)?;
        if let Some(document) = self.live_drain()? {
            return Err(AdmissionError::TestsDraining {
                reason: drain_reason(&document),
            });
        }
        Ok(StartGuard {
            admission: self,
            file_lock,
            state,
            worktree,
        })
    }

    /// Hold the cross-process admission lock until the returned guard drops.
    pub fn start_guard(&self) -> Result<StartGuard<'_>, AdmissionError> {
        self.start_guard_with(None)
    }

    /// Serialize starts for one worktree before entering the global drain gate.
    /// Waiting uses a condition variable and never a polling timer.
    pub fn start_guard_for(&self, worktree_id: &str) -> Result<StartGuard<'_>, AdmissionError> {
        let mut active = self
            .worktree_starts
            .lock()
            .map_err(|_| AdmissionError::StatePoisoned)?;
        while active.contains(worktree_id) {
            active = self
                .worktree_changed
                .wait(active)
                .map_err(|_| AdmissionError::StatePoisoned)?;
        }
        active.insert(worktree_id.to_owned());
        drop(active);
        self.start_guard_with(Some(WorktreeStartPermit {
            admission: self,
            worktree_id: worktree_id.to_owned(),
        }))
    }

    pub(crate) fn try_worktree_guard(
        &self,
        worktree_id: &str,
    ) -> Result<Option<WorktreeStartPermit<'_>>, AdmissionError> {
        let mut active = self
            .worktree_starts
            .lock()
            .map_err(|_| AdmissionError::StatePoisoned)?;
        if !active.insert(worktree_id.to_owned()) {
            return Ok(None);
        }
        Ok(Some(WorktreeStartPermit {
            admission: self,
            worktree_id: worktree_id.to_owned(),
        }))
    }

    fn with_critical<T>(
        &self,
        operation: impl FnOnce(&mut AdmissionState) -> Result<T, AdmissionError>,
    ) -> Result<T, AdmissionError> {
        let mut state = self.state()?;
        let _file_lock = ExclusiveFileLock::acquire(&self.lock_file)?;
        operation(&mut state)
    }

    fn write_activity_locked(&self, state: &mut AdmissionState) -> Result<(), AdmissionError> {
        state.generation = state.generation.checked_add(1).ok_or_else(|| {
            filesystem(
                "advance test activity generation",
                io::Error::new(io::ErrorKind::InvalidData, "activity generation overflow"),
            )
        })?;
        let active = state
            .active
            .iter()
            .map(|(run_id, unit)| json!({"run_id": run_id, "unit": unit}))
            .collect::<Vec<_>>();
        atomic_write_json(
            &self.runtime,
            ACTIVITY_FILE,
            &json!({
                "schema": 1,
                "generation": state.generation,
                "active": active,
            }),
            0o644,
        )
    }

    fn started_locked(
        &self,
        state: &mut AdmissionState,
        run_id: &str,
        unit: &str,
    ) -> Result<(), AdmissionError> {
        state.active.insert(run_id.to_owned(), unit.to_owned());
        if let Err(error) = self.write_activity_locked(state) {
            state.active.remove(run_id);
            return Err(error);
        }
        Ok(())
    }

    fn finished_locked(
        &self,
        state: &mut AdmissionState,
        run_id: &str,
    ) -> Result<(), AdmissionError> {
        if state.active.remove(run_id).is_some() {
            self.write_activity_locked(state)?;
        }
        Ok(())
    }

    pub fn started(&self, run_id: &str, unit: &str) -> Result<(), AdmissionError> {
        self.with_critical(|state| self.started_locked(state, run_id, unit))
    }

    pub fn finished(&self, run_id: &str) -> Result<(), AdmissionError> {
        self.with_critical(|state| self.finished_locked(state, run_id))
    }

    pub fn reset(&self) -> Result<(), AdmissionError> {
        self.with_critical(|state| {
            state.active.clear();
            self.write_activity_locked(state)
        })
    }

    pub fn accepted_activity_count(&self) -> Result<usize, AdmissionError> {
        Ok(self.state()?.active.len())
    }
}

/// RAII serialization boundary for one accepted test start.
pub struct StartGuard<'a> {
    admission: &'a TestAdmission,
    #[allow(dead_code)]
    file_lock: ExclusiveFileLock<'a>,
    state: MutexGuard<'a, AdmissionState>,
    #[allow(dead_code)]
    worktree: Option<WorktreeStartPermit<'a>>,
}

pub(crate) struct WorktreeStartPermit<'a> {
    admission: &'a TestAdmission,
    worktree_id: String,
}

impl Drop for WorktreeStartPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .admission
            .worktree_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.worktree_id);
        drop(active);
        self.admission.worktree_changed.notify_all();
    }
}

impl<'a> StartGuard<'a> {
    pub(crate) fn release_global(self) -> Option<WorktreeStartPermit<'a>> {
        let Self {
            worktree,
            state,
            file_lock,
            ..
        } = self;
        drop(file_lock);
        drop(state);
        worktree
    }
    pub fn started(&mut self, run_id: &str, unit: &str) -> Result<(), AdmissionError> {
        self.admission.started_locked(&mut self.state, run_id, unit)
    }

    pub fn finished(&mut self, run_id: &str) -> Result<(), AdmissionError> {
        self.admission.finished_locked(&mut self.state, run_id)
    }

    pub fn reset(&mut self) -> Result<(), AdmissionError> {
        self.state.active.clear();
        self.admission.write_activity_locked(&mut self.state)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainLease {
    pub runtime_dir: PathBuf,
    pub nonce: String,
}

pub fn begin_drain(
    runtime_dir: impl AsRef<Path>,
    reason: &str,
) -> Result<DrainLease, AdmissionError> {
    let runtime = RuntimeDirectory::open(runtime_dir.as_ref(), true)?;
    let lock_file = runtime.open_lock()?;
    let _lock = ExclusiveFileLock::acquire(&lock_file)?;
    if read_json(&runtime, DRAIN_FILE)
        .as_ref()
        .is_some_and(lease_is_live)
    {
        return Err(AdmissionError::DrainConflict);
    }
    unlink_if_present(&runtime, DRAIN_FILE, "remove prior test drain lease")?;
    let pid = i64::from(std::process::id());
    let process_start = process_start(pid).ok_or(AdmissionError::ProcessIdentityUnavailable)?;
    let nonce = random_hex(16)?;
    atomic_write_json(
        &runtime,
        DRAIN_FILE,
        &json!({
            "schema": 1,
            "pid": pid,
            "process_start": process_start,
            "nonce": nonce,
            "reason": reason,
            "created_at": iso_now()?,
        }),
        0o600,
    )?;
    Ok(DrainLease {
        runtime_dir: runtime.path,
        nonce,
    })
}

pub fn end_drain(lease: &DrainLease) -> Result<(), AdmissionError> {
    let runtime = RuntimeDirectory::open(&lease.runtime_dir, true)?;
    let lock_file = runtime.open_lock()?;
    let _lock = ExclusiveFileLock::acquire(&lock_file)?;
    let owned = read_json(&runtime, DRAIN_FILE)
        .and_then(|document| {
            document
                .get("nonce")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|nonce| nonce == lease.nonce);
    if owned {
        unlink_if_present(&runtime, DRAIN_FILE, "remove owned test drain lease")?;
    }
    Ok(())
}

fn read_activity_at(runtime: &RuntimeDirectory) -> Option<ActivityReceipt> {
    let document = read_json(runtime, ACTIVITY_FILE)?;
    if document.get("schema").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let active = document.get("active")?.as_array()?.clone();
    Some(ActivityReceipt {
        schema: 1,
        generation: document.get("generation").and_then(Value::as_u64),
        active,
    })
}

pub fn read_activity(
    runtime_dir: impl AsRef<Path>,
) -> Result<Option<ActivityReceipt>, AdmissionError> {
    match RuntimeDirectory::open(runtime_dir.as_ref(), false) {
        Ok(runtime) => Ok(read_activity_at(&runtime)),
        Err(AdmissionError::InvalidRuntimePath) => Err(AdmissionError::InvalidRuntimePath),
        Err(_) => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn notify_runtime_mutation(_runtime_dir: &Path) {}

#[cfg(not(target_os = "linux"))]
fn runtime_notifiers() -> &'static Mutex<BTreeMap<PathBuf, std::sync::Weak<tokio::sync::Notify>>> {
    static NOTIFIERS: std::sync::OnceLock<
        Mutex<BTreeMap<PathBuf, std::sync::Weak<tokio::sync::Notify>>>,
    > = std::sync::OnceLock::new();
    NOTIFIERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(not(target_os = "linux"))]
fn runtime_notifier(runtime_dir: &Path) -> std::sync::Arc<tokio::sync::Notify> {
    let mut notifiers = runtime_notifiers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(notifier) = notifiers
        .get(runtime_dir)
        .and_then(std::sync::Weak::upgrade)
    {
        return notifier;
    }
    let notifier = std::sync::Arc::new(tokio::sync::Notify::new());
    notifiers.insert(
        runtime_dir.to_path_buf(),
        std::sync::Arc::downgrade(&notifier),
    );
    notifier
}

#[cfg(not(target_os = "linux"))]
fn notify_runtime_mutation(runtime_dir: &Path) {
    let notifier = runtime_notifier(runtime_dir);
    notifier.notify_waiters();
}

#[cfg(target_os = "linux")]
struct DirectoryEvents {
    descriptor: tokio::io::unix::AsyncFd<OwnedFd>,
}

#[cfg(target_os = "linux")]
impl DirectoryEvents {
    fn new(runtime: &RuntimeDirectory) -> Result<Self, AdmissionError> {
        // SAFETY: `inotify_init1` has no pointer arguments and the returned fd
        // is immediately checked before ownership is transferred to `OwnedFd`.
        let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if raw < 0 {
            return Err(AdmissionError::EventWatch(io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a newly created, uniquely owned descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
        let proc_path = CString::new(format!("/proc/self/fd/{}", runtime.descriptor.as_raw_fd()))
            .expect("numeric descriptor paths contain no NUL bytes");
        // SAFETY: the C string is NUL-terminated and lives for the duration of
        // the call; `descriptor` remains open and valid.
        let watch = unsafe {
            libc::inotify_add_watch(descriptor.as_raw_fd(), proc_path.as_ptr(), INOTIFY_EVENTS)
        };
        if watch < 0 {
            return Err(AdmissionError::EventWatch(io::Error::last_os_error()));
        }
        let descriptor =
            tokio::io::unix::AsyncFd::new(descriptor).map_err(AdmissionError::EventWatch)?;
        Ok(Self { descriptor })
    }

    async fn wait(&self) -> Result<(), AdmissionError> {
        loop {
            let mut ready = self
                .descriptor
                .readable()
                .await
                .map_err(AdmissionError::EventWatch)?;
            match ready.try_io(|source| {
                let mut buffer = [0_u8; 65_536];
                rustix::io::read(source.get_ref(), &mut buffer).map_err(io::Error::from)
            }) {
                Ok(Ok(0)) => {
                    return Err(AdmissionError::EventWatch(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "inotify descriptor closed",
                    )));
                }
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(error)) => return Err(AdmissionError::EventWatch(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Wait for an atomic activity receipt mutation; time never counts as success.
#[cfg(target_os = "linux")]
pub async fn wait_for_zero_activity(runtime_dir: impl AsRef<Path>) -> Result<(), AdmissionError> {
    let runtime = RuntimeDirectory::open(runtime_dir.as_ref(), false)?;
    let events = DirectoryEvents::new(&runtime)?;
    loop {
        let activity = read_activity_at(&runtime).ok_or(AdmissionError::ActivityUnavailable)?;
        if activity.is_zero() {
            return Ok(());
        }
        events.wait().await?;
    }
}

/// Portable same-process fallback for platforms where the daemon is not
/// deployed. Every mutation notifies registered waiters; there is no timer.
#[cfg(not(target_os = "linux"))]
pub async fn wait_for_zero_activity(runtime_dir: impl AsRef<Path>) -> Result<(), AdmissionError> {
    let runtime = RuntimeDirectory::open(runtime_dir.as_ref(), false)?;
    let notifier = runtime_notifier(&runtime.path);
    loop {
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let activity = read_activity_at(&runtime).ok_or(AdmissionError::ActivityUnavailable)?;
        if activity.is_zero() {
            return Ok(());
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn activity(runtime: &Path) -> ActivityReceipt {
        read_activity(runtime).unwrap().expect("activity receipt")
    }

    #[test]
    fn drain_closes_admission_and_activity_is_exact() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();
        {
            let mut guard = admission.start_guard().unwrap();
            guard.started("t1", "unit-1").unwrap();
        }
        assert_eq!(
            activity(temporary.path()).active_tests().unwrap(),
            vec![ActiveTest {
                run_id: "t1".to_owned(),
                unit: "unit-1".to_owned(),
            }]
        );
        let lease = begin_drain(temporary.path(), "upgrade").unwrap();
        let error = admission.start_guard().err().expect("drain rejects start");
        assert_eq!(error.kind(), AdmissionErrorKind::TestsDraining);
        assert!(error.to_string().contains("upgrade"));
        admission.finished("t1").unwrap();
        assert!(activity(temporary.path()).active.is_empty());
        end_drain(&lease).unwrap();
        drop(admission.start_guard().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_zero_activity_wakes_from_atomic_receipt_event() {
        let temporary = tempdir().unwrap();
        let admission = Arc::new(TestAdmission::new(temporary.path()).unwrap());
        admission.reset().unwrap();
        admission.started("t1", "unit-1").unwrap();
        let runtime = temporary.path().to_path_buf();
        let waiting = tokio::spawn(async move { wait_for_zero_activity(runtime).await });
        tokio::task::yield_now().await;
        admission.finished("t1").unwrap();
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("event-driven wait completed")
            .expect("wait task joined")
            .expect("zero activity observed");
    }

    #[test]
    fn stale_drain_lease_is_recovered_on_next_start() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();
        std::fs::write(
            temporary.path().join(DRAIN_FILE),
            b"{\"nonce\":\"stale\",\"pid\":999999999,\"process_start\":\"1\",\"reason\":\"old upgrade\",\"schema\":1}\n",
        )
        .unwrap();
        drop(admission.start_guard().unwrap());
        assert!(!temporary.path().join(DRAIN_FILE).exists());

        let pid = i64::from(std::process::id());
        let reused_identity = format!("{}-reused", process_start(pid).unwrap());
        std::fs::write(
            temporary.path().join(DRAIN_FILE),
            serde_json::to_vec(&json!({
                "schema": 1,
                "pid": pid,
                "process_start": reused_identity,
                "nonce": "stale-pid-reuse",
                "reason": "old upgrade",
            }))
            .unwrap(),
        )
        .unwrap();
        drop(admission.start_guard().unwrap());
        assert!(!temporary.path().join(DRAIN_FILE).exists());
    }

    #[test]
    fn second_live_drain_is_refused() {
        let temporary = tempdir().unwrap();
        TestAdmission::new(temporary.path())
            .unwrap()
            .reset()
            .unwrap();
        let lease = begin_drain(temporary.path(), "first").unwrap();
        let document: Value =
            serde_json::from_slice(&std::fs::read(temporary.path().join(DRAIN_FILE)).unwrap())
                .unwrap();
        assert_eq!(document.get("schema"), Some(&json!(1)));
        assert_eq!(document.get("pid"), Some(&json!(std::process::id())));
        assert_eq!(
            document.get("process_start").and_then(Value::as_str),
            process_start(i64::from(std::process::id())).as_deref()
        );
        assert_eq!(
            document.get("nonce").and_then(Value::as_str).map(str::len),
            Some(32)
        );
        let created = document.get("created_at").and_then(Value::as_str).unwrap();
        assert_eq!(created.len(), 20);
        assert!(created.ends_with('Z'));
        let error = begin_drain(temporary.path(), "second").unwrap_err();
        assert_eq!(error.kind(), AdmissionErrorKind::DrainConflict);
        assert_eq!(error.to_string(), "another live test drain already exists");
        end_drain(&lease).unwrap();
    }

    #[test]
    fn start_guard_serializes_drain_creation_without_polling() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();
        let guard = admission.start_guard().unwrap();
        let runtime = temporary.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            sender.send(begin_drain(runtime, "serialized")).unwrap();
        });
        barrier.wait();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(guard);
        let lease = receiver.recv().unwrap().unwrap();
        worker.join().unwrap();
        end_drain(&lease).unwrap();
    }

    #[test]
    fn worktree_start_guard_serializes_the_same_worktree_without_a_timer() {
        let temporary = tempdir().unwrap();
        let admission = Arc::new(TestAdmission::new(temporary.path()).unwrap());
        admission.reset().unwrap();
        let guard = admission.start_guard_for("worktree-1").unwrap();
        let worker_admission = Arc::clone(&admission);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            let acquired = worker_admission.start_guard_for("worktree-1").is_ok();
            sender.send(acquired).unwrap();
        });
        barrier.wait();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(guard);
        assert!(receiver.recv().unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn activity_generation_sorting_and_idempotent_finish_are_exact() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();
        assert_eq!(
            std::fs::read_to_string(temporary.path().join(ACTIVITY_FILE)).unwrap(),
            "{\"active\":[],\"generation\":1,\"schema\":1}\n"
        );
        admission.started("z-run", "unit-z").unwrap();
        admission.started("a-run", "unit-a").unwrap();
        let receipt = activity(temporary.path());
        assert_eq!(receipt.generation, Some(3));
        let runs = receipt
            .active_tests()
            .unwrap()
            .into_iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>();
        assert_eq!(runs, ["a-run", "z-run"]);
        admission.finished("missing").unwrap();
        assert_eq!(activity(temporary.path()).generation, Some(3));
        admission.finished("z-run").unwrap();
        assert_eq!(activity(temporary.path()).generation, Some(4));
    }

    #[test]
    fn failed_activity_publication_rolls_back_the_new_run_and_preserves_generation() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();
        std::fs::remove_file(temporary.path().join(ACTIVITY_FILE)).unwrap();
        std::fs::create_dir(temporary.path().join(ACTIVITY_FILE)).unwrap();
        assert!(admission.started("unpublished", "unit").is_err());
        std::fs::remove_dir(temporary.path().join(ACTIVITY_FILE)).unwrap();
        admission.reset().unwrap();
        let receipt = activity(temporary.path());
        assert_eq!(receipt.generation, Some(3));
        assert!(receipt.active.is_empty());
    }

    #[test]
    fn drain_ownership_and_invalid_document_cleanup_are_exact() {
        let temporary = tempdir().unwrap();
        let admission = TestAdmission::new(temporary.path()).unwrap();
        admission.reset().unwrap();

        std::fs::write(temporary.path().join(DRAIN_FILE), b"not-json\n").unwrap();
        drop(admission.start_guard().unwrap());
        assert_eq!(
            std::fs::read(temporary.path().join(DRAIN_FILE)).unwrap(),
            b"not-json\n"
        );

        let lease = begin_drain(temporary.path(), "").unwrap();
        let error = admission.start_guard().err().unwrap();
        assert_eq!(error.to_string(), "coordinator upgrade in progress");
        let foreign = DrainLease {
            runtime_dir: lease.runtime_dir.clone(),
            nonce: "not-the-owner".to_owned(),
        };
        end_drain(&foreign).unwrap();
        assert!(temporary.path().join(DRAIN_FILE).is_file());
        end_drain(&lease).unwrap();
        assert!(!temporary.path().join(DRAIN_FILE).exists());

        let names = std::fs::read_dir(temporary.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!names.iter().any(|name| name.starts_with(".test-")));
    }

    #[test]
    fn activity_reader_preserves_v1_shape_acceptance() {
        let temporary = tempdir().unwrap();
        TestAdmission::new(temporary.path()).unwrap();
        std::fs::write(
            temporary.path().join(ACTIVITY_FILE),
            b"{\"active\":[1],\"schema\":1}\n",
        )
        .unwrap();
        let receipt = read_activity(temporary.path()).unwrap().unwrap();
        assert_eq!(receipt.generation, None);
        assert_eq!(receipt.active, vec![json!(1)]);
        assert!(receipt.active_tests().is_none());
        std::fs::write(
            temporary.path().join(ACTIVITY_FILE),
            b"{\"active\":[],\"schema\":2}\n",
        )
        .unwrap();
        assert!(read_activity(temporary.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn unavailable_activity_is_an_error_not_time_based_success() {
        let temporary = tempdir().unwrap();
        TestAdmission::new(temporary.path()).unwrap();
        let error = wait_for_zero_activity(temporary.path()).await.unwrap_err();
        assert_eq!(error.kind(), AdmissionErrorKind::ActivityUnavailable);
        assert_eq!(error.to_string(), "test activity receipt is unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_files_have_exact_modes_and_atomic_writes_replace_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempdir().unwrap();
        let outside = temporary.path().join("outside");
        std::fs::write(&outside, b"unchanged\n").unwrap();
        let runtime = temporary.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        symlink(&outside, runtime.join(ACTIVITY_FILE)).unwrap();
        let admission = TestAdmission::new(&runtime).unwrap();
        admission.reset().unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged\n");
        assert!(
            !std::fs::symlink_metadata(runtime.join(ACTIVITY_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let lease = begin_drain(&runtime, "mode check").unwrap();
        let mode = |name: &str| {
            std::fs::metadata(runtime.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(LOCK_FILE), 0o666);
        assert_eq!(mode(ACTIVITY_FILE), 0o644);
        assert_eq!(mode(DRAIN_FILE), 0o600);
        end_drain(&lease).unwrap();

        std::fs::remove_file(runtime.join(LOCK_FILE)).unwrap();
        symlink(&outside, runtime.join(LOCK_FILE)).unwrap();
        let error = TestAdmission::new(&runtime).err().unwrap();
        assert_eq!(error.kind(), AdmissionErrorKind::Filesystem);
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged\n");
    }

    #[test]
    fn parent_traversal_is_rejected_before_runtime_creation() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("parent").join("..").join("runtime");
        let error = TestAdmission::new(path).err().unwrap();
        assert_eq!(error.kind(), AdmissionErrorKind::InvalidRuntimePath);
    }
}
