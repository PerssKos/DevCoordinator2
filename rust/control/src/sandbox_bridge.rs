//! File-backed transport for clients running where socket syscalls are denied.
//!
//! The normal local API remains the Unix socket. This bridge is deliberately a
//! transport adapter only: it carries the same protocol envelope, never adds
//! authorization or scheduling, and derives the local caller from the request
//! file owner. The default directory is a root-owned sticky directory in the
//! host temporary filesystem, which is writable by sandboxed callers without
//! requiring a network namespace or socket syscall.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use devcoordinator2_api::{
    ErrorCode, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, ProtocolError, ResponseEnvelope,
    encode_response, parse_request,
};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{error, info};

use crate::daemon::{App, PeerCredentials};

pub const DEFAULT_DIR: &str = "/tmp/devcoordinator2-bridge";
pub const DEFAULT_SOCKET: &str = "/run/devcoordinator2/daemon.sock";
const MAX_IN_FLIGHT: usize = 64;
const RESPONSE_TTL: Duration = Duration::from_secs(15 * 60);
const MIN_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_millis(500);

pub fn default_directory(socket_path: &Path) -> PathBuf {
    if socket_path == Path::new(DEFAULT_SOCKET) {
        PathBuf::from(DEFAULT_DIR)
    } else {
        socket_path.with_file_name("sandbox-bridge")
    }
}

pub async fn serve(
    app: std::sync::Arc<App>,
    directory: &Path,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    serve_with_owner(app, directory, shutdown, unsafe { libc::geteuid() }).await
}

async fn serve_with_owner(
    app: std::sync::Arc<App>,
    directory: &Path,
    mut shutdown: watch::Receiver<bool>,
    expected_owner: u32,
) -> io::Result<()> {
    prepare_directory(directory, expected_owner)?;
    let lock = acquire_lock(directory, expected_owner)?;
    recover_processing(directory)?;
    info!(path = %directory.display(), "serving sandbox request bridge");
    let mut work = JoinSet::new();
    let mut poll = MIN_POLL;
    loop {
        while work.try_join_next().is_some() {}
        let claimed = scan_requests(directory, &app, &mut work, &shutdown)?;
        if claimed > 0 {
            poll = MIN_POLL;
        } else {
            poll = (poll * 2).min(MAX_POLL);
        }
        cleanup_responses(directory);
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            () = sleep(poll) => {}
        }
    }
    while work.join_next().await.is_some() {}
    drop(lock);
    Ok(())
}

fn acquire_lock(directory: &Path, expected_owner: u32) -> io::Result<std::fs::File> {
    let path = directory.join("daemon.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != expected_owner || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sandbox bridge lock has an unexpected owner",
        ));
    }
    let result = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if result != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another daemon owns the sandbox bridge",
        ));
    }
    Ok(file)
}

fn prepare_directory(directory: &Path, expected_owner: u32) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != expected_owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sandbox bridge directory must be owned by the daemon",
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o1777 != 0o1777 || mode & 0o6000 != 0 {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o1777))?;
    }
    Ok(())
}

fn scan_requests(
    directory: &Path,
    app: &std::sync::Arc<App>,
    work: &mut JoinSet<()>,
    shutdown: &watch::Receiver<bool>,
) -> io::Result<usize> {
    let mut claimed = 0;
    for entry in fs::read_dir(directory)? {
        if work.len() >= MAX_IN_FLIGHT {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("request") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        let processing = directory.join(format!("{id}.processing"));
        if fs::rename(&path, &processing).is_err() {
            continue;
        }
        claimed += 1;
        let app = std::sync::Arc::clone(app);
        let directory = directory.to_owned();
        let id = id.to_owned();
        let shutdown = shutdown.clone();
        work.spawn(async move {
            if let Err(error) = process_claimed(&directory, &id, &processing, app, shutdown).await {
                error!(request = %id, %error, "sandbox request bridge failed");
            }
        });
    }
    Ok(claimed)
}

async fn process_claimed(
    directory: &Path,
    id: &str,
    processing: &Path,
    app: std::sync::Arc<App>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let (owner, raw) = match read_claimed(processing) {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_file(processing);
            return Err(error);
        }
    };
    let peer = PeerCredentials {
        pid: 0,
        uid: owner.0,
        gid: owner.1,
    };
    let response = match parse_request(&raw) {
        Ok(request) if request.id == id => {
            let event_wait = request.operation == "event.wait";
            if event_wait {
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stopped| *stopped) => {
                        ResponseEnvelope::failure(id, ProtocolError::new(
                            ErrorCode::DaemonUnavailable,
                            "coordinator is restarting; re-query the operation with its last cursor",
                        ))
                    }
                    _ = app.wait_for_installation_fence() => {
                        ResponseEnvelope::failure(id, ProtocolError::new(
                            ErrorCode::DaemonUnavailable,
                            "installation in progress; reconnect the event wait with its last cursor",
                        ))
                    }
                    response = app.dispatch(request, peer) => response,
                }
            } else {
                app.dispatch(request, peer).await
            }
        }
        Ok(_) => ResponseEnvelope::failure(
            id,
            ProtocolError::new(
                ErrorCode::ProtocolInvalid,
                "bridge request id does not match its file",
            ),
        ),
        Err(error) => ResponseEnvelope::failure(id, error),
    };
    // Retain the actual result and retry only persistence, never dispatch.
    // A full filesystem used to leave a completed operation looking active
    // forever, until restart. Keep the request if shutdown prevents delivery.
    let mut delay = MIN_POLL;
    loop {
        match write_response(directory, id, owner, &response) {
            Ok(()) => break,
            Err(error) => {
                if *shutdown.borrow() {
                    return Err(error);
                }
                tokio::select! {
                    _ = shutdown.wait_for(|stopped| *stopped) => return Err(error),
                    () = sleep(delay) => { delay = (delay * 2).min(MAX_POLL); }
                }
            }
        }
    }
    fs::remove_file(processing)?;
    Ok(())
}

fn read_claimed(path: &Path) -> io::Result<((u32, u32), Vec<u8>)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > (MAX_REQUEST_BYTES + 1) as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bridge request is not a bounded regular file",
        ));
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
    Ok(((metadata.uid(), metadata.gid()), raw))
}

fn write_response(
    directory: &Path,
    id: &str,
    owner: (u32, u32),
    response: &ResponseEnvelope,
) -> io::Result<()> {
    let temporary = directory.join(format!(".{id}.response.{}.tmp", std::process::id()));
    let path = directory.join(format!("{id}.response"));
    let bytes = encode_response(response);
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bridge response exceeds protocol cap",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let published = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        if unsafe {
            libc::fchown(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                owner.0,
                owner.1,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        fs::rename(&temporary, path)
    })();
    drop(file);
    if published.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    published
}

fn recover_processing(directory: &Path) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("processing") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            let _ = fs::remove_file(path);
            continue;
        };
        if !valid_id(id) {
            let _ = fs::remove_file(path);
            continue;
        }
        let owner = match read_claimed(&path) {
            Ok((owner, raw)) => {
                let request_id = serde_json::from_slice::<Value>(&raw)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned));
                if request_id.as_deref() == Some(id) {
                    owner
                } else {
                    (0, 0)
                }
            }
            Err(_) => (0, 0),
        };
        if owner != (0, 0) && !directory.join(format!("{id}.response")).exists() {
            let response = ResponseEnvelope::failure(
                id,
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "request was interrupted by daemon restart; re-query the operation status",
                ),
            );
            // Do not discard the only accepted-request receipt if recovery is
            // still unable to persist its reconnectable failure response.
            write_response(directory, id, owner, &response)?;
        }
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn cleanup_responses(directory: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("response") {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > RESPONSE_TTL)
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn valid_id(value: &str) -> bool {
    value.len() <= 64 && !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::OperationExecutor;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    struct PendingObserver(tokio::sync::Notify);
    impl OperationExecutor for PendingObserver {
        fn execute(
            &self,
            _: &str,
            _: Value,
            _: &crate::access::Caller,
        ) -> Result<Value, ProtocolError> {
            unreachable!("observer is deferred")
        }
        fn defer(
            &self,
            operation: &str,
            _: Value,
            _: &crate::access::Caller,
        ) -> Option<Result<crate::daemon::DeferredOperation, ProtocolError>> {
            assert_eq!(operation, "event.wait");
            self.0.notify_one();
            Some(Ok(Box::pin(std::future::pending())))
        }
    }

    fn claimed_request(directory: &Path, id: &str, operation: &str) -> PathBuf {
        let path = directory.join(format!("{id}.processing"));
        let params = match operation {
            "event.wait" => json!({"filters":[{"filter_id":"fixture","categories":["health"]}]}),
            "task.create" => json!({"title":"fixture mutation","kind":"improvement"}),
            _ => json!({}),
        };
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "protocol":2,"id":id,"operation":operation,"params":params,"client":{}
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn bridge_observer_receives_cutover_and_can_reconnect_after_fence_removal() {
        let temporary = tempdir().unwrap();
        let directory = temporary.path();
        let endpoint = directory.join("daemon.sock");
        let executor = Arc::new(PendingObserver(tokio::sync::Notify::new()));
        let app =
            Arc::new(App::with_executor(None, executor.clone()).with_installation_fence(endpoint));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let processing = claimed_request(directory, "abcdef", "event.wait");
        let dir = directory.to_owned();
        let observer_app = app.clone();
        let observer = tokio::spawn(async move {
            process_claimed(&dir, "abcdef", &processing, observer_app, shutdown).await
        });
        tokio::time::timeout(Duration::from_secs(2), executor.0.notified())
            .await
            .unwrap();
        fs::write(directory.join("daemon.pre-cutover.sock"), "fixture fence").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), observer).await;
        // Ensure a failing baseline cannot leave an endless task behind.
        let _ = shutdown_tx.send(true);
        result
            .expect("bridge observer was not interrupted by cutover")
            .unwrap()
            .unwrap();
        let response: ResponseEnvelope =
            serde_json::from_slice(&fs::read(directory.join("abcdef.response")).unwrap()).unwrap();
        assert!(
            matches!(response, ResponseEnvelope::Failure { error, .. } if error.code == ErrorCode::DaemonUnavailable)
        );
        assert!(!directory.join("abcdef.processing").exists());
        fs::remove_file(directory.join("daemon.pre-cutover.sock")).unwrap();
        let (stop, shutdown) = watch::channel(false);
        let processing = claimed_request(directory, "abcdee", "event.wait");
        let dir = directory.to_owned();
        let reconnected = tokio::spawn(async move {
            process_claimed(&dir, "abcdee", &processing, app, shutdown).await
        });
        tokio::time::timeout(Duration::from_secs(2), executor.0.notified())
            .await
            .unwrap();
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), reconnected)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    struct CountingExecutor(std::sync::atomic::AtomicUsize);
    impl OperationExecutor for CountingExecutor {
        fn execute(
            &self,
            _: &str,
            _: Value,
            _: &crate::access::Caller,
        ) -> Result<Value, ProtocolError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({"committed":true}))
        }
    }

    #[tokio::test]
    async fn response_persistence_failure_retries_result_without_replaying_mutation() {
        for shutdown_while_failed in [false, true] {
            let temporary = tempdir().unwrap();
            let directory = temporary.path();
            let executor = Arc::new(CountingExecutor(std::sync::atomic::AtomicUsize::new(0)));
            let app = Arc::new(App::with_executor(None, executor.clone()));
            let processing = claimed_request(directory, "abcdef", "task.create");
            // Atomic response publication fails, exactly as a filesystem write fault.
            fs::create_dir(directory.join("abcdef.response")).unwrap();
            let (stop, shutdown) = watch::channel(false);
            let dir = directory.to_owned();
            let worker = tokio::spawn(async move {
                process_claimed(&dir, "abcdef", &processing, app, shutdown).await
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                while executor.0.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            sleep(Duration::from_millis(80)).await;
            assert!(
                !worker.is_finished(),
                "persistence failure abandoned the accepted result"
            );
            assert!(directory.join("abcdef.processing").exists());
            assert!(
                !directory
                    .join(format!(".abcdef.response.{}.tmp", std::process::id()))
                    .exists()
            );
            if shutdown_while_failed {
                stop.send(true).unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_secs(2), worker)
                        .await
                        .unwrap()
                        .unwrap()
                        .is_err()
                );
                assert!(directory.join("abcdef.processing").exists());
            } else {
                fs::remove_dir(directory.join("abcdef.response")).unwrap();
                tokio::time::timeout(Duration::from_secs(2), worker)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let response: ResponseEnvelope =
                    serde_json::from_slice(&fs::read(directory.join("abcdef.response")).unwrap())
                        .unwrap();
                assert!(response.is_ok());
                assert!(!directory.join("abcdef.processing").exists());
            }
            assert_eq!(executor.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    struct RecordingExecutor(Arc<Mutex<Option<(u32, u32)>>>);

    #[test]
    fn restart_response_write_failure_preserves_the_accepted_request() {
        let temporary = tempdir().unwrap();
        let path = claimed_request(temporary.path(), "aabbcc", "task.create");
        let blocked = temporary
            .path()
            .join(format!(".aabbcc.response.{}.tmp", std::process::id()));
        fs::create_dir(&blocked).unwrap();
        assert!(recover_processing(temporary.path()).is_err());
        assert!(path.exists());
        fs::remove_dir(&blocked).unwrap();
        recover_processing(temporary.path()).unwrap();
        assert!(!path.exists());
        let response: ResponseEnvelope =
            serde_json::from_slice(&fs::read(temporary.path().join("aabbcc.response")).unwrap())
                .unwrap();
        assert!(
            matches!(response, ResponseEnvelope::Failure { error, .. } if error.code == ErrorCode::DaemonUnavailable)
        );
    }

    struct FiniteMutation {
        started: tokio::sync::Notify,
        release: Arc<tokio::sync::Notify>,
    }
    impl OperationExecutor for FiniteMutation {
        fn execute(
            &self,
            _: &str,
            _: Value,
            _: &crate::access::Caller,
        ) -> Result<Value, ProtocolError> {
            unreachable!()
        }
        fn defer(
            &self,
            _: &str,
            _: Value,
            _: &crate::access::Caller,
        ) -> Option<Result<crate::daemon::DeferredOperation, ProtocolError>> {
            self.started.notify_one();
            let release = self.release.clone();
            Some(Ok(Box::pin(async move {
                release.notified().await;
                Ok(json!({"committed":true}))
            })))
        }
    }

    #[tokio::test]
    async fn fence_does_not_cancel_an_accepted_bridge_mutation() {
        let temporary = tempdir().unwrap();
        let executor = Arc::new(FiniteMutation {
            started: tokio::sync::Notify::new(),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let app = Arc::new(
            App::with_executor(None, executor.clone())
                .with_installation_fence(temporary.path().join("daemon.sock")),
        );
        let processing = claimed_request(temporary.path(), "aabbcc", "task.create");
        let (_stop, shutdown) = watch::channel(false);
        let directory = temporary.path().to_owned();
        let worker = tokio::spawn(async move {
            process_claimed(&directory, "aabbcc", &processing, app, shutdown).await
        });
        tokio::time::timeout(Duration::from_secs(2), executor.started.notified())
            .await
            .unwrap();
        fs::write(
            temporary.path().join("daemon.pre-cutover.sock"),
            "fixture fence",
        )
        .unwrap();
        sleep(Duration::from_millis(50)).await;
        assert!(!worker.is_finished());
        assert!(temporary.path().join("aabbcc.processing").exists());
        executor.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let response: ResponseEnvelope =
            serde_json::from_slice(&fs::read(temporary.path().join("aabbcc.response")).unwrap())
                .unwrap();
        assert!(response.is_ok());
    }
    impl OperationExecutor for RecordingExecutor {
        fn execute(
            &self,
            operation: &str,
            params: Value,
            caller: &crate::access::Caller,
        ) -> Result<Value, ProtocolError> {
            *self.0.lock().unwrap() = Some((caller.uid, caller.gid));
            if operation != "ping" {
                return Err(ProtocolError::new(ErrorCode::OperationUnknown, "fixture"));
            }
            serde_json::from_value::<devcoordinator2_api::EmptyParams>(params)
                .map_err(|_| ProtocolError::new(ErrorCode::ParamsInvalid, "fixture"))?;
            Ok(json!({"accepted":true}))
        }
    }

    #[tokio::test]
    async fn dispatches_files_and_uses_the_request_owner_as_caller() {
        let temporary = tempdir().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(RecordingExecutor(observed.clone())),
        ));
        let (signal, shutdown) = watch::channel(false);
        let dir = temporary.path().to_owned();
        let bridge_dir = dir.clone();
        let bridge = tokio::spawn(async move {
            serve_with_owner(app, &bridge_dir, shutdown, unsafe { libc::geteuid() }).await
        });
        let id = "a1b2c3";
        let request = serde_json::json!({"protocol":2,"id":id,"operation":"ping","params":{},"client":{"kind":"codex"}});
        fs::write(
            dir.join(format!("{id}.request")),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(raw) = fs::read(dir.join(format!("{id}.response"))) {
                    break raw;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&response).unwrap();
        assert!(response.is_ok());
        assert_eq!(
            *observed.lock().unwrap(),
            Some((unsafe { libc::geteuid() }, unsafe { libc::getegid() }))
        );
        signal.send(true).unwrap();
        bridge.await.unwrap().unwrap();
    }

    #[test]
    fn interrupted_processing_is_reported_without_replaying_the_request() {
        let temporary = tempdir().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let id = "deadbeef";
        let request = serde_json::json!({"protocol":2,"id":id,"operation":"ping","params":{}});
        fs::write(
            temporary.path().join(format!("{id}.processing")),
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        recover_processing(temporary.path()).unwrap();
        let raw = fs::read(temporary.path().join(format!("{id}.response"))).unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&raw).unwrap();
        assert!(!response.is_ok());
        assert!(!temporary.path().join(format!("{id}.processing")).exists());
    }

    #[test]
    fn custom_socket_instances_get_isolated_bridge_directories() {
        assert_eq!(
            default_directory(Path::new(DEFAULT_SOCKET)),
            Path::new(DEFAULT_DIR)
        );
        assert_eq!(
            default_directory(Path::new("/var/tmp/isolated/daemon.sock")),
            Path::new("/var/tmp/isolated/sandbox-bridge")
        );
    }
}
