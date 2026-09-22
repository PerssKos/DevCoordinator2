use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use devcoordinator2_api::{
    ClientContext, ErrorCode, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, ProtocolError, RequestEnvelope,
    ResponseEnvelope,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Instant, sleep, timeout};

pub async fn call(
    socket_path: &Path,
    operation: impl Into<String>,
    params: Value,
    client: ClientContext,
) -> Result<ResponseEnvelope, ProtocolError> {
    let operation = operation.into();
    let review_action = matches!(
        operation.as_str(),
        "review.prepare" | "review.record" | "performance.overview" | "performance.review"
    );
    let blocking_wait = operation == "event.wait";
    let long_running_action = matches!(
        operation.as_str(),
        "deployment.apply"
            | "deployment.rollback"
            | "deployment.start"
            | "deployment.stop"
            | "deployment.restart"
            | "deployment.remove"
            | "deployment.recovery"
            | "plan.recovery"
    );
    let request = RequestEnvelope {
        protocol: PROTOCOL_VERSION,
        id: request_id(),
        operation,
        params,
        client,
    };
    let mut stream = match timeout(Duration::from_secs(5), UnixStream::connect(socket_path)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if error.raw_os_error() == Some(libc::EPERM) => {
            let bridge_directory = crate::config::sandbox_bridge_directory(socket_path);
            return call_via_sandbox_bridge(
                &request,
                &bridge_directory,
                blocking_wait,
                long_running_action,
                review_action,
            )
            .await;
        }
        Ok(Err(error)) => {
            return Err(ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                format!("cannot reach daemon at {}", socket_path.display()),
            )
            .with_detail(error.to_string()));
        }
        Err(_) => {
            return Err(ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "daemon connection timed out",
            ));
        }
    };
    let mut encoded = serde_json::to_vec(&request).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "cannot encode request")
            .with_detail(error.to_string())
    })?;
    encoded.push(b'\n');
    timeout(Duration::from_secs(5), stream.write_all(&encoded))
        .await
        .map_err(|_| {
            ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "daemon request write timed out",
            )
        })?
        .map_err(transport_error)?;
    if !blocking_wait {
        timeout(Duration::from_secs(5), stream.shutdown())
            .await
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "daemon request shutdown timed out",
                )
            })?
            .map_err(transport_error)?;
    }

    let mut response = Vec::new();
    let mut limited = (&mut stream).take((MAX_RESPONSE_BYTES + 1) as u64);
    let read = limited.read_to_end(&mut response);
    if blocking_wait || long_running_action {
        read.await.map_err(transport_error)?;
    } else {
        let reply_seconds = if review_action { 20 } else { 10 };
        timeout(Duration::from_secs(reply_seconds), read)
            .await
            .map_err(|_| {
                ProtocolError::new(ErrorCode::DaemonUnavailable, "daemon response timed out")
            })?
            .map_err(transport_error)?;
    }
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "daemon response exceeds size cap",
        ));
    }
    serde_json::from_slice(&response).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "daemon response is invalid")
            .with_detail(error.to_string())
    })
}

async fn call_via_sandbox_bridge(
    request: &RequestEnvelope,
    directory: &Path,
    blocking_wait: bool,
    long_running_action: bool,
    review_action: bool,
) -> Result<ResponseEnvelope, ProtocolError> {
    let id = &request.id;
    let request_path = directory.join(format!("{id}.request"));
    let response_path = directory.join(format!("{id}.response"));
    let mut encoded = serde_json::to_vec(request).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "cannot encode request")
            .with_detail(error.to_string())
    })?;
    encoded.push(b'\n');
    if encoded.len() > devcoordinator2_api::MAX_REQUEST_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::RequestTooLarge,
            "request exceeds 64 KiB frame cap",
        ));
    }
    write_bridge_request(directory, &request_path, &encoded)?;
    let deadline = if blocking_wait || long_running_action {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(if review_action { 20 } else { 10 }))
    };
    let mut delay = Duration::from_millis(10);
    loop {
        match read_bridge_response(&response_path) {
            Ok(bytes) => {
                let _ = tokio::fs::remove_file(&response_path).await;
                return decode_bridge_response(&bytes, id);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                let _ = std::fs::remove_file(&response_path);
                return Err(ProtocolError::new(
                    ErrorCode::ProtocolInvalid,
                    error.to_string(),
                ));
            }
            Err(error) => return Err(bridge_error("cannot read sandbox bridge response", error)),
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "sandbox bridge response timed out; re-query accepted operations",
            ));
        }
        sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(100));
    }
}

fn read_bridge_response(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > (MAX_RESPONSE_BYTES + 1) as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bridge response is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn decode_bridge_response(
    bytes: &[u8],
    expected_id: &str,
) -> Result<ResponseEnvelope, ProtocolError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "daemon response exceeds size cap",
        ));
    }
    let response: ResponseEnvelope = serde_json::from_slice(bytes).map_err(|error| {
        ProtocolError::new(ErrorCode::ProtocolInvalid, "daemon response is invalid")
            .with_detail(error.to_string())
    })?;
    let id = match &response {
        ResponseEnvelope::Success { id, .. } | ResponseEnvelope::Failure { id, .. } => id,
    };
    if id != expected_id {
        return Err(ProtocolError::new(
            ErrorCode::ProtocolInvalid,
            "daemon response id does not match request",
        ));
    }
    Ok(response)
}

fn write_bridge_request(
    directory: &Path,
    request: &Path,
    bytes: &[u8],
) -> Result<(), ProtocolError> {
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| bridge_error("sandbox bridge is unavailable", error))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o1777 != 0o1777 {
        return Err(ProtocolError::new(
            ErrorCode::DaemonUnavailable,
            "sandbox bridge is unavailable",
        ));
    }
    let temporary = request.with_extension(format!("request.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|error| bridge_error("cannot create sandbox bridge request", error))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(bridge_error("cannot write sandbox bridge request", error));
    }
    drop(file);
    std::fs::rename(temporary, request)
        .map_err(|error| bridge_error("cannot publish sandbox bridge request", error))
}

fn bridge_error(message: &str, error: std::io::Error) -> ProtocolError {
    ProtocolError::new(ErrorCode::DaemonUnavailable, message).with_detail(error.to_string())
}

fn transport_error(error: std::io::Error) -> ProtocolError {
    ProtocolError::new(ErrorCode::DaemonUnavailable, "daemon connection failed")
        .with_detail(error.to_string())
}

fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}{:x}", SEQUENCE.fetch_add(1, Ordering::Relaxed))
        .chars()
        .take(24)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::App;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn sandbox_bridge_client_round_trip_preserves_the_protocol_envelope() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o1777))
            .unwrap();
        let (signal, shutdown) = tokio::sync::watch::channel(false);
        let directory = temporary.path().to_owned();
        let bridge_directory = directory.clone();
        let bridge = tokio::spawn(async move {
            crate::sandbox_bridge::serve(
                Arc::new(App::new(Path::new("/run/test-daemon.sock"))),
                &bridge_directory,
                shutdown,
            )
            .await
        });
        let request = RequestEnvelope {
            protocol: PROTOCOL_VERSION,
            id: "a1b2c3d4".into(),
            operation: "ping".into(),
            params: serde_json::json!({}),
            client: ClientContext::default(),
        };
        let response = call_via_sandbox_bridge(&request, &directory, false, false, false)
            .await
            .unwrap();
        assert!(response.is_ok());
        signal.send(true).unwrap();
        bridge.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn deployment_waits_for_the_result_beyond_the_ordinary_deadline() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut encoded = String::new();
            stream.read_line(&mut encoded).await.unwrap();
            let request: RequestEnvelope = serde_json::from_str(&encoded).unwrap();
            let mut finished = Vec::new();
            stream.read_to_end(&mut finished).await.unwrap();
            tokio::time::advance(Duration::from_secs(11)).await;
            let response =
                ResponseEnvelope::success(request.id, serde_json::json!({"finished": true}))
                    .unwrap();
            stream
                .get_mut()
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
        });
        let result = call(
            &socket,
            "deployment.apply",
            serde_json::json!({}),
            ClientContext::default(),
        )
        .await;
        server.await.unwrap();
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_waits_for_verified_large_snapshot_results() {
        for operation in ["plan.recovery", "deployment.recovery"] {
            let temporary = tempfile::tempdir().unwrap();
            let socket = temporary.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut encoded = String::new();
                stream.read_line(&mut encoded).await.unwrap();
                let request: RequestEnvelope = serde_json::from_str(&encoded).unwrap();
                let mut finished = Vec::new();
                stream.read_to_end(&mut finished).await.unwrap();
                tokio::time::advance(Duration::from_secs(11)).await;
                let response =
                    ResponseEnvelope::success(request.id, serde_json::json!({"finished": true}))
                        .unwrap();
                stream
                    .get_mut()
                    .write_all(&serde_json::to_vec(&response).unwrap())
                    .await
                    .unwrap();
            });
            let result = call(
                &socket,
                operation,
                serde_json::json!({}),
                ClientContext::default(),
            )
            .await;
            server.await.unwrap();
            assert!(result.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn recovery_bridge_keeps_waiting_for_the_same_accepted_request() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o1777))
            .unwrap();
        let directory = temporary.path().to_owned();
        let pending_directory = directory.clone();
        let request = RequestEnvelope {
            protocol: PROTOCOL_VERSION,
            id: "recovery1234".into(),
            operation: "deployment.recovery".into(),
            params: serde_json::json!({}),
            client: ClientContext::default(),
        };
        let pending = tokio::spawn(async move {
            call_via_sandbox_bridge(&request, &pending_directory, false, true, false).await
        });
        for _ in 0..1024 {
            if directory.join("recovery1234.request").exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(directory.join("recovery1234.request").exists());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        let response =
            ResponseEnvelope::success("recovery1234", serde_json::json!({"status":"prepared"}))
                .unwrap();
        std::fs::write(
            directory.join("recovery1234.response"),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
        tokio::time::resume();
        assert!(pending.await.unwrap().unwrap().is_ok());
    }

    #[tokio::test]
    async fn review_reply_budget_is_bounded_without_extending_receipt_lookups() {
        for (operation, delay_seconds, succeeds) in [
            ("review.prepare", 16, true),
            ("review.record", 16, true),
            ("review.prepare", 21, false),
            ("review.receipt", 11, false),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let socket = temporary.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
            let (response_sender, response_receiver) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut encoded = String::new();
                stream.read_line(&mut encoded).await.unwrap();
                let request: RequestEnvelope = serde_json::from_str(&encoded).unwrap();
                let mut finished = Vec::new();
                stream.read_to_end(&mut finished).await.unwrap();
                ready_sender.send(()).unwrap();
                response_receiver.await.unwrap();
                let response =
                    ResponseEnvelope::success(request.id, serde_json::json!({"partial":true}))
                        .unwrap();
                let _ = stream
                    .get_mut()
                    .write_all(&serde_json::to_vec(&response).unwrap())
                    .await;
            });
            let pending = tokio::spawn(async move {
                call(
                    &socket,
                    operation,
                    serde_json::json!({}),
                    ClientContext::default(),
                )
                .await
            });
            ready_receiver.await.unwrap();
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(delay_seconds)).await;
            let result = if succeeds {
                tokio::time::resume();
                response_sender.send(()).unwrap();
                pending.await.unwrap()
            } else {
                let result = pending.await.unwrap();
                tokio::time::resume();
                response_sender.send(()).unwrap();
                result
            };
            server.await.unwrap();
            assert_eq!(
                result.is_ok(),
                succeeds,
                "{operation} at {delay_seconds}s: {result:?}"
            );
        }
    }
}
