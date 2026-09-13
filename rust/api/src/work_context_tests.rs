use super::*;
use crate::{ClientContext, parse_request};
use serde_json::json;

fn valid() -> serde_json::Value {
    json!({"version":1,"native_project_id":"native-clock-hash","thread_id":"thread-1","turn_id":"turn-1","operation_id":"operation-1","workstream_id":"implementation","outcome_id":"outcome-1","experiment_ref":"review-1@2"})
}

#[test]
fn work_context_locked_envelope_round_trips_and_preserves_legacy_omission() {
    let work = WorkContext::parse(&valid().to_string()).unwrap();
    assert_eq!(serde_json::to_value(&work).unwrap(), valid());
    let client = ClientContext {
        work: Some(work.clone()),
        work_source: Some(WorkSource::Environment),
        ..ClientContext::default()
    };
    assert_eq!(
        client.work_attribution(),
        Some(WorkAttribution {
            context: Some(work),
            source: WorkSource::Environment,
            diagnostic: None
        })
    );
    assert_eq!(
        serde_json::to_value(ClientContext::default()).unwrap(),
        json!({"kind":"other"})
    );
    assert_eq!(ClientContext::default().work_attribution(), None);
}

#[test]
fn work_context_bad_metadata_is_omitted_without_cancelling_or_echoing_it() {
    for work in [
        json!({"version":2,"native_project_id":"secret-marker","thread_id":"thread"}),
        json!({"version":1,"native_project_id":"native","thread_id":"thread","headers":"secret-marker"}),
        json!("secret-marker"),
        json!({"version":1,"native_project_id":"","thread_id":"thread"}),
    ] {
        let raw = json!({"protocol":2,"id":"request","operation":"ping","params":{},"client":{"kind":"codex","work":work}});
        let request = parse_request(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(request.operation, "ping");
        assert!(request.client.work.is_none());
        assert_eq!(
            request.client.work_diagnostic,
            Some(WorkDiagnostic::Invalid)
        );
        assert!(
            !serde_json::to_string(&request)
                .unwrap()
                .contains("secret-marker")
        );
    }
}

#[test]
fn work_context_enforces_byte_bounds_and_session_conflict_without_authority_changes() {
    let mut input = valid();
    input["operation_id"] = json!("x".repeat(256));
    assert!(WorkContext::parse(&input.to_string()).is_ok());
    input["operation_id"] = json!("x".repeat(257));
    assert_eq!(
        WorkContext::parse(&input.to_string()),
        Err(WorkDiagnostic::Invalid)
    );
    assert_eq!(
        WorkContext::parse(&" ".repeat(2049)),
        Err(WorkDiagnostic::TooLarge)
    );
    let client = ClientContext {
        session: Some("different-session".into()),
        work: Some(WorkContext::parse(&valid().to_string()).unwrap()),
        ..ClientContext::default()
    };
    assert_eq!(
        client.work_attribution(),
        Some(WorkAttribution {
            context: None,
            source: WorkSource::Request,
            diagnostic: Some(WorkDiagnostic::SessionConflict)
        })
    );
    assert_eq!(client.identity, None);
}
