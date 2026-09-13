use std::path::Path;
use std::time::Duration;

use devcoordinator2_api::{
    ClientContext, ErrorCode, MAX_RESPONSE_BYTES, PROTOCOL_VERSION, ProtocolError, RequestEnvelope,
    ResponseEnvelope,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

pub async fn call(
    socket_path: &Path,
    operation: impl Into<String>,
    params: Value,
    client: ClientContext,
) -> Result<ResponseEnvelope, ProtocolError> {
    let operation = operation.into();
    let review_action = matches!(operation.as_str(), "review.prepare" | "review.record");
    let blocking_wait = operation == "event.wait";
    let deployment_action = matches!(
        operation.as_str(),
        "deployment.apply"
            | "deployment.rollback"
            | "deployment.start"
            | "deployment.stop"
            | "deployment.restart"
            | "deployment.remove"
    );
    let mut stream = timeout(Duration::from_secs(5), UnixStream::connect(socket_path))
        .await
        .map_err(|_| {
            ProtocolError::new(ErrorCode::DaemonUnavailable, "daemon connection timed out")
        })?
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                format!("cannot reach daemon at {}", socket_path.display()),
            )
            .with_detail(error.to_string())
        })?;
    let request = RequestEnvelope {
        protocol: PROTOCOL_VERSION,
        id: request_id(),
        operation,
        params,
        client,
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
    if blocking_wait || deployment_action {
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
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

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
