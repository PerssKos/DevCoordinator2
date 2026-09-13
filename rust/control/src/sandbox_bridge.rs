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
use std::path::Path;
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
const MAX_IN_FLIGHT: usize = 64;
const RESPONSE_TTL: Duration = Duration::from_secs(15 * 60);
const MIN_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_millis(500);

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
    recover_processing(directory)?;
    info!(path = %directory.display(), "serving sandbox request bridge");
    let mut work = JoinSet::new();
    let mut poll = MIN_POLL;
    loop {
        while work.try_join_next().is_some() {}
        let claimed = scan_requests(directory, &app, &mut work)?;
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
    Ok(())
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
        work.spawn(async move {
            if let Err(error) = process_claimed(&directory, &id, &processing, app).await {
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
        Ok(request) if request.id == id => app.dispatch(request, peer).await,
        Ok(_) => ResponseEnvelope::failure(
            id,
            ProtocolError::new(
                ErrorCode::ProtocolInvalid,
                "bridge request id does not match its file",
            ),
        ),
        Err(error) => ResponseEnvelope::failure(id, error),
    };
    write_response(directory, id, owner, &response)?;
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let bytes = encode_response(response);
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bridge response exceeds protocol cap",
        ));
    }
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
        let error = io::Error::last_os_error();
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    fs::rename(temporary, path)
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
            let _ = write_response(directory, id, owner, &response);
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

    struct RecordingExecutor(Arc<Mutex<Option<(u32, u32)>>>);
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
}
