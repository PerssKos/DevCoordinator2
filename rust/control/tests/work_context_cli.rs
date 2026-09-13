use devcoordinator2_api::work_context::WorkDiagnostic;
use devcoordinator2_api::{RequestEnvelope, ResponseEnvelope};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn work_context_real_cli_environment_is_bounded_nonfatal_and_session_aware() {
    let work = json!({"version":1,"native_project_id":"native-clock-hash","thread_id":"thread-1","turn_id":"turn-1","operation_id":"operation-1","workstream_id":"implementation","outcome_id":"outcome-1","experiment_ref":"review-1@2"}).to_string();
    for (environment, session, diagnostic) in [
        (work.clone(), None, None),
        (
            work.clone(),
            Some("different"),
            Some(WorkDiagnostic::SessionConflict),
        ),
        (
            "private-marker invalid".into(),
            None,
            Some(WorkDiagnostic::Invalid),
        ),
        ("x".repeat(2049), None, Some(WorkDiagnostic::TooLarge)),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("fixture.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut stream = BufReader::new(stream);
            let mut encoded = String::new();
            stream.read_line(&mut encoded).await.unwrap();
            let request: RequestEnvelope =
                devcoordinator2_api::parse_request(encoded.as_bytes()).unwrap();
            let response =
                ResponseEnvelope::success(request.id.clone(), json!({"accepted":true})).unwrap();
            stream
                .get_mut()
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            request
        });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_devcoordinator2"));
        command
            .current_dir(temporary.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(
                "DEVCOORDINATOR2_INSTANCE_ENV",
                temporary.path().join("absent.env"),
            )
            .env("DEVCOORDINATOR2_SOCKET", &socket)
            .env("DEVCOORDINATOR_WORK_CONTEXT", environment)
            .args(["ping", "--client", "human", "--format", "json"]);
        if let Some(session) = session {
            command.args(["--session", session]);
        }
        let output = command.output().await.unwrap();
        let request = server.await.unwrap();
        assert!(output.status.success());
        assert_eq!(request.operation, "ping");
        assert_eq!(request.params, json!({}));
        assert_eq!(request.client.kind, devcoordinator2_api::ClientKind::Human);
        assert_eq!(request.client.work_diagnostic, diagnostic);
        assert_eq!(request.client.work.is_some(), diagnostic.is_none());
        if let Some(diagnostic) = diagnostic {
            assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic.code()));
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-marker"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["data"],
            json!({"accepted":true})
        );
    }
}
