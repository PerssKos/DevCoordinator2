use super::*;
use crate::usage_api_fixture::{KEY, capability, response};

#[test]
fn api_contract_requires_negotiation_and_complete_consistent_fresh_metadata() {
    validate_capability(&json!({"localUsageAccounting":capability()})).unwrap();
    assert!(validate_capability(&json!({})).is_err());
    let mut cap = capability();
    cap["version"] = json!(2);
    assert!(validate_capability(&json!({"localUsageAccounting":cap})).is_err());
    let params = json!({"repositoryKey":KEY,"fromAt":10,"toAt":20});
    let value = response(&params, 20, 135);
    let summary: Summary = serde_json::from_value(value.clone()).unwrap();
    validate(&summary, Some(KEY), 10, 20).unwrap();
    let mut later_response = value.clone();
    later_response["generatedAt"] = json!(21);
    let later: Summary = serde_json::from_value(later_response).unwrap();
    validate(&later, Some(KEY), 10, 20).unwrap();
    assert_eq!(later.snapshot.as_ref().unwrap().generated_at, 20);
    assert_eq!(
        later
            .source_report(Some(KEY), 1, None, true)
            .source_snapshot
            .unwrap()
            .generated_at_ms,
        20
    );
    assert_eq!(
        summary.snapshot.as_ref().unwrap().journal_watermark,
        Some(10)
    );
    for field in [
        "sourceWatermark",
        "generation",
        "journalWatermark",
        "watermarkAtMs",
        "updatedAtMs",
        "pendingEntries",
        "pendingCorrections",
    ] {
        let mut missing = value.clone();
        missing["snapshot"].as_object_mut().unwrap().remove(field);
        let summary: Summary = serde_json::from_value(missing).unwrap();
        assert_eq!(
            validate(&summary, Some(KEY), 10, 20).unwrap_err(),
            "api_contract_unsupported",
            "{field}"
        );
    }
    for (field, bad) in [
        ("journalWatermark", json!(9)),
        ("generatedAt", json!(21)),
        ("updatedAtMs", json!(21)),
        ("generation", json!(0)),
    ] {
        let mut inconsistent = value.clone();
        inconsistent["snapshot"][field] = bad;
        let summary: Summary = serde_json::from_value(inconsistent).unwrap();
        assert_eq!(
            validate(&summary, Some(KEY), 10, 20).unwrap_err(),
            "api_snapshot_inconsistent",
            "{field}"
        );
    }
}

#[test]
fn source_projection_keeps_unknown_only_tokens_unknown_and_preserves_snapshot() {
    let mut value = response(&json!({"repositoryKey":KEY,"fromAt":10,"toAt":20}), 20, 135);
    let summary: Summary = serde_json::from_value(value.clone()).unwrap();
    let report = summary.source_report(Some(KEY), 1, None, true);
    assert_eq!(report.tokens["total_tokens"], 135);
    let metadata = report.source_snapshot.unwrap();
    assert_eq!(metadata.generation, 7);
    assert_eq!(metadata.pending_entries, 0);
    let mut stale = value.clone();
    stale["snapshot"]["freshness"] = json!("stale");
    stale["snapshot"]["pendingEntries"] = json!(97);
    let summary: Summary = serde_json::from_value(stale.clone()).unwrap();
    let report = summary.source_report(Some(KEY), 1, None, true);
    let snapshot = report.snapshot.unwrap();
    assert!(snapshot.refreshing);
    assert!(
        !snapshot.refresh_failed,
        "stale publication with a successful read is not a failed refresh"
    );
    assert_eq!(report.source_snapshot.unwrap().freshness, "stale");
    let failed =
        SummaryRead::cached_or_error(Some(Arc::new(summary)), "api_contract_unsupported").unwrap();
    assert!(
        failed
            .source_report(Some(KEY), 1, None, true)
            .snapshot
            .unwrap()
            .refresh_failed
    );
    stale["snapshot"]["freshness"] = json!("failed");
    let failed: Summary = serde_json::from_value(stale).unwrap();
    assert!(
        failed
            .source_report(Some(KEY), 1, None, true)
            .snapshot
            .unwrap()
            .refresh_failed
    );
    for token in value["report"]["providerTokens"].as_array_mut().unwrap() {
        token["measuredTokens"] = json!(0);
        token["exactTokens"] = Value::Null;
        token["unknownObservations"] = json!(2);
    }
    let summary: Summary = serde_json::from_value(value).unwrap();
    assert!(
        !summary
            .source_report(Some(KEY), 1, None, true)
            .tokens
            .contains_key("total_tokens")
    );
}

#[test]
fn explicit_review_rows_keep_unknown_precision_costs_and_truthful_time_basis() {
    let value: Value = serde_json::from_str(include_str!(
        "../../../test-fixtures/usage-v2-bound-response.json"
    ))
    .unwrap();
    let mut summary: Summary = serde_json::from_value(value).unwrap();
    validate(&summary, Some("repo-a"), 0, 3000).unwrap();
    let review = summary.review.as_mut().unwrap();
    review.outcomes.totals.provider_total_tokens.exact = None;
    review.outcomes.totals.provider_total_tokens.unknown = 0;
    review.outcomes.totals.elapsed_execution_ms.measured = 1000;
    review.outcomes.totals.cost.status = "partial".into();
    review.outcomes.totals.cost.estimated_usd_micros = Some(7);
    let mut groups = super::review_aggregate::from_api(&review.outcomes);
    groups
        .merge(super::review_aggregate::from_api(&review.outcomes), 1)
        .unwrap();
    let report = groups.finish(false).unwrap();
    assert_eq!(report.totals.provider_total_tokens.exact, None);
    assert_eq!(report.totals.cost.estimated_usd_micros, Some(14));
    assert_eq!(report.totals.elapsed_execution_ms.exact, None);
    assert!(
        report
            .basis
            .contains("aggregate intervals cannot be unioned")
    );
    assert_eq!(report.rows.len(), 1);
}

#[test]
fn independent_healthy_collectors_do_not_hold_the_global_cache_lock_during_ipc() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let left = root.join("left");
    let right = root.join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    let gate = ResponseGate::default();
    let (entered, responses) = mpsc::channel();
    let left_gate = gate.clone();
    let left_entered = entered.clone();
    let a = Peer::start(&left, &left.join("home"), move |params| {
        left_entered.send(()).unwrap();
        left_gate.wait();
        response(params, 20, 135)
    });
    let right_gate = gate.clone();
    let b = Peer::start(&right, &right.join("home"), move |params| {
        entered.send(()).unwrap();
        right_gate.wait();
        response(params, 20, 135)
    });
    let release = gate.release_on_drop();
    let api = CollectorApi::default();
    let barrier = std::sync::Barrier::new(2);
    let run = |source| {
        barrier.wait();
        api.summary(
            source,
            Some(KEY),
            None,
            true,
            10,
            20,
            Instant::now() + Duration::from_secs(2),
        )
    };
    std::thread::scope(|scope| {
        let left = scope.spawn(|| run(&a.source));
        let right = scope.spawn(|| run(&b.source));
        let first = responses.recv_timeout(Duration::from_secs(1));
        let second = responses.recv_timeout(Duration::from_secs(1));
        release.release();
        assert!(
            first.is_ok() && second.is_ok(),
            "both collectors must reach IPC before either response is released"
        );
        assert!(left.join().unwrap().is_ok());
        assert!(right.join().unwrap().is_ok());
    });
}
