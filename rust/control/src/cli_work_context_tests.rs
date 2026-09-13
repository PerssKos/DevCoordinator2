use super::*;

const WORK: &str = r#"{"version":1,"native_project_id":"native-clock-hash","thread_id":"thread-1","turn_id":"turn-1","operation_id":"operation-1","workstream_id":"implementation","outcome_id":"outcome-1","experiment_ref":"review-1@2"}"#;

#[test]
fn work_context_cli_uses_exact_environment_shape_and_explicit_flags_win() {
    let cli = Cli::try_parse_from(["devcoordinator2", "ping", "--client", "human"]).unwrap();
    let context = cli.client_context_with_work(Some(OsStr::new(WORK)));
    assert_eq!(
        context,
        ClientContext {
            kind: ClientKind::Human,
            session: Some("thread-1".into()),
            work: Some(WorkContext::parse(WORK).unwrap()),
            work_source: Some(WorkSource::Environment),
            ..ClientContext::default()
        }
    );
    assert_eq!(cli.client_context_with_work(None).work, None);
    for (session, retained) in [("thread-1", true), ("explicit-other", false)] {
        let cli = Cli::try_parse_from([
            "devcoordinator2",
            "ping",
            "--session",
            session,
            "--client",
            "other",
        ])
        .unwrap();
        let context = cli.client_context_with_work(Some(OsStr::new(WORK)));
        assert_eq!(context.kind, ClientKind::Other);
        assert_eq!(context.session.as_deref(), Some(session));
        assert_eq!(context.work.is_some(), retained);
        assert_eq!(
            context.work_diagnostic,
            (!retained).then_some(WorkDiagnostic::SessionConflict)
        );
    }
}

#[test]
fn work_context_cli_bad_environment_never_changes_or_cancels_the_command() {
    for raw in [
        "secret-marker malformed",
        r#"{"version":2,"native_project_id":"native","thread_id":"thread"}"#,
        r#"{"version":1,"native_project_id":"native","thread_id":"thread","clientKind":"edge"}"#,
    ] {
        let cli = Cli::try_parse_from(["devcoordinator2", "ping"]).unwrap();
        let context = cli.client_context_with_work(Some(OsStr::new(raw)));
        assert_eq!(context.work, None);
        assert_eq!(context.work_source, Some(WorkSource::Environment));
        assert_eq!(context.work_diagnostic, Some(WorkDiagnostic::Invalid));
        assert!(
            !serde_json::to_string(&context)
                .unwrap()
                .contains("secret-marker")
        );
        assert!(matches!(
            cli.into_invocation().unwrap(),
            Invocation::Remote {
                operation: "ping",
                ..
            }
        ));
    }
}
