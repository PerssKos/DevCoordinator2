use super::*;
use crate::automation_test_support::Fixture;
use crate::config::CodexUsageSource;
use crate::repository::Registry;
use serde_json::json;
use std::sync::Arc;

const BEFORE: &str = "t20260903T120718Z-57067c";
const AFTER: &str = "t20260903T120719Z-57067d";

fn run_ref(run: &str) -> EvidenceRef {
    EvidenceRef {
        kind: EvidenceKind::Run,
        reference: format!("w1111111111111111/{run}"),
    }
}

fn measured_fixture() -> (Fixture, Record, u64) {
    let mut fixture = Fixture::new();
    let mut request = super::tests::record(&fixture);
    let home = fixture.repository.parent().unwrap().join("usage-source");
    let (canonical, now) = crate::usage::tests::source_database(&home, 5);
    let uid = rustix::process::getuid().as_raw();
    let source = rusqlite::Connection::open(home.join("usage/usage.sqlite3")).unwrap();
    source.execute("INSERT INTO operations VALUES('result-op','model_request','agent-private',?1,'implementation','coding','model_active','agent_declared')", [(now-40_000) as i64]).unwrap();
    source
        .execute(
            "INSERT INTO operation_events VALUES('result-op',1,?1,'completed')",
            [(now - 30_000) as i64],
        )
        .unwrap();
    source.execute_batch("INSERT INTO model_requests VALUES('result-request','result-op'); INSERT INTO effective_classification_events VALUES('result-op','implementation','coding','model_active','agent_declared');").unwrap();
    source
        .execute(
            "INSERT INTO repository_attributions VALUES('result-op',?1)",
            [&canonical],
        )
        .unwrap();
    source.execute("INSERT INTO token_observations VALUES('total_tokens',60,'complete',?1,'result-request',NULL,?2,'provider_reported')", rusqlite::params![(now-30_000) as i64,canonical]).unwrap();
    source
        .execute(
            "INSERT INTO coverage_events VALUES('result-op','complete',?1)",
            [(now - 30_000) as i64],
        )
        .unwrap();
    fixture
        .database
        .call(move |connection| {
            connection.execute(
                "INSERT INTO codex_usage_repository_links VALUES(?1,'project-alpha',?2,5,1,'t')",
                rusqlite::params![uid, canonical],
            )?;
            Ok(())
        })
        .unwrap();
    fixture.config.codex_usage_sources = vec![CodexUsageSource {
        uid,
        codex_home: home,
        executable: fixture.repository.join("unused-probe"),
    }];
    fixture.service = ReviewService::new(
        fixture.database.clone(),
        UsageService::with_clock(
            fixture.config.clone(),
            fixture.database.clone(),
            Registry::new(fixture.database.clone()),
            Arc::new(crate::platform::HostClock),
        ),
    );
    let directory = fixture.repository.join(".devcoordinator/test");
    std::fs::create_dir_all(&directory).unwrap();
    let runs = [
        (BEFORE, now - 58_000, now - 52_000),
        (AFTER, now - 25_000, now - 20_000),
    ];
    std::fs::write(directory.join("history.json"), serde_json::to_vec(&json!({"schema":2,"runs":runs.iter().map(|(run,start,end)| json!({"run_id":run,"test":"focused","status":"passed","started_at":timestamp(*start).unwrap(),"finished_at":timestamp(*end).unwrap(),"duration_seconds":5.0,"exit_code":0})).collect::<Vec<_>>()})).unwrap()).unwrap();
    std::fs::write(directory.join("evidence.json"), serde_json::to_vec(&json!({"schema":2,"runs":runs.iter().map(|(run,_,_)| json!({"run_id":run,"test":"focused","proof":"selected","status":"passed","source_digest":"a".repeat(64),"config_digest":"b".repeat(64),"requested_tier":"development","readiness_eligible":false,"selection":[],"checks":[]})).collect::<Vec<_>>()})).unwrap()).unwrap();
    request.record.window_start_ms = now - 60_000;
    request.record.window_end_ms = now - 49_999;
    let before = fixture
        .service
        .prepare(prepare(&request.record), now)
        .unwrap();
    request.record.experiment.baseline.evidence_refs =
        vec![before.source_refs[0].clone(), run_ref(BEFORE)];
    request.record.experiment.baseline.missing_measurements = before.coverage_gaps;
    request.record.experiment.baseline.interpretation =
        "One measured model request used 100 provider-reported tokens before the change.".into();
    request.record.experiment.result_evidence_refs = vec![
        usage_ref("project-alpha", now - 49_999, now),
        run_ref(AFTER),
    ];
    request.record.experiment.comparison = Some(Comparison { narrative: "The same focused workload used 100 tokens before and 60 after; its passing validation preserves quality. This is an evidence-backed interpretation, not machine-proved causality.".into(), input_change_reason: None });
    request.record.experiment.disposition = Disposition::Retained;
    request.record.experiment.hypothesis = "Reusing unchanged configuration removes a redundant tool read from the same bounded workload.".into();
    request.record.experiment.chosen_action =
        "Reuse parsed configuration for identical inputs while preserving all required validation."
            .into();
    request.record.experiment.success_criteria = "Reduce observed tokens for the same request while its focused quality verification still passes.".into();
    request.record.experiment.rollback_condition =
        "Revert reuse if changed inputs become stale or the required quality verification fails."
            .into();
    request.record.experiment.reason = "The before/after source records show fewer tokens and one less tool call for the same workload; the result run passes.".into();
    request.record.experiment.observations[0].kind = ObservationKind::AvoidableWork;
    request.record.experiment.observations[0].interpretation = "The baseline repeats a configuration read that the result avoids. Intentional repeated quality validation is retained, not classified as waste.".into();
    let after = fixture
        .service
        .usage
        .review_window(
            &fixture.service.repository("project-alpha").unwrap(),
            now - 49_999,
            now,
            Instant::now() + QUERY_TIMEOUT,
        )
        .unwrap();
    assert_eq!(
        (before.usage.totals.total_tokens, after.totals.total_tokens),
        (Some(100), Some(60))
    );
    if after.time.summed_agent_active.unknown_intervals > 0 {
        request
            .record
            .experiment
            .baseline
            .missing_measurements
            .push("result_active_time_partial".into());
    }
    (fixture, request, now)
}

fn prepare(record: &ReviewRecord) -> Prepare {
    Prepare {
        repository_id: record.repository_id.clone(),
        workstream_id: record.workstream_id.clone(),
        window_start_ms: record.window_start_ms,
        window_end_ms: record.window_end_ms,
        offset: 0,
        limit: 10,
        before_decision_seq: None,
    }
}

fn change_run(fixture: &Fixture, file: &str, field: &str, value: serde_json::Value) {
    let path = fixture.repository.join(".devcoordinator/test").join(file);
    let mut data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    data["runs"][1][field] = value;
    std::fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
}

#[test]
fn review_result_single_record_compares_real_windows_and_keeps_intentional_repeat_valid() {
    for disposition in [Disposition::Retained, Disposition::Reverted] {
        let (fixture, mut request, now) = measured_fixture();
        request.record.experiment.disposition = disposition;
        request.record.experiment.observations[0]
            .evidence_refs
            .push(request.record.experiment.result_evidence_refs[0].clone());
        let expected = request.record.clone();
        let result = fixture.service.record(request, "fixture", now).unwrap();
        assert!(result.completed);
        assert_eq!(result.revision, 1);
        assert_eq!(result.record, expected);
        assert_eq!(
            fixture
                .service
                .receipt(Reference {
                    reference: result.reference.clone()
                })
                .unwrap(),
            result
        );
    }
}

#[test]
fn review_result_rejects_historical_future_failed_or_missing_quality_times() {
    for (field, value) in [
        ("finished_at", json!("1970-01-02T00:01:00Z")),
        ("finished_at", json!("2099-01-01T00:00:00Z")),
        ("finished_at", json!(null)),
        ("status", json!("failed")),
    ] {
        let (fixture, request, now) = measured_fixture();
        change_run(&fixture, "history.json", field, value);
        assert!(
            fixture.service.record(request, "fixture", now).is_err(),
            "{field}"
        );
    }
}

#[test]
fn review_result_rejects_wrong_repository_overlapping_future_or_noncanonical_usage_refs() {
    let (fixture, request, now) = measured_fixture();
    for reference in [
        format!("canonical-usage:other:{}:{now}", now - 40_000),
        format!("canonical-usage:project-alpha:{}:{now}", now - 50_000),
        format!("canonical-usage:project-alpha:{}:{}", now - 40_000, now + 1),
        format!("canonical-usage:project-alpha:0{}:{now}", now - 40_000),
        "canonical-usage:project-alpha:invalid:now".into(),
    ] {
        let mut changed = request.clone();
        changed.record.experiment.result_evidence_refs[0].reference = reference;
        assert!(fixture.service.record(changed, "fixture", now).is_err());
    }
    let mut changed = request;
    changed.record.experiment.baseline.evidence_refs[0] =
        changed.record.experiment.result_evidence_refs[0].clone();
    assert!(fixture.service.record(changed, "fixture", now).is_err());
}

#[test]
fn review_result_requires_comparison_measurements_and_quality_not_just_a_passing_run() {
    let (fixture, request, now) = measured_fixture();
    for mode in 0..3 {
        let mut changed = request.clone();
        match mode {
            0 => changed.record.experiment.comparison = None,
            1 => {
                changed.record.experiment.result_evidence_refs.remove(0);
            }
            2 => {
                changed.record.experiment.result_evidence_refs.pop();
            }
            _ => unreachable!(),
        }
        assert!(fixture.service.record(changed, "fixture", now).is_err());
    }
}

#[test]
fn review_result_checks_retained_workload_and_explains_changed_inputs_without_current_head() {
    let (fixture, mut request, now) = measured_fixture();
    change_run(
        &fixture,
        "evidence.json",
        "source_digest",
        json!("c".repeat(64)),
    );
    assert!(
        fixture
            .service
            .record(request.clone(), "fixture", now)
            .is_err()
    );
    request.record.experiment.comparison.as_mut().unwrap().input_change_reason = Some("The repository-local optimization changes the source while preserving the same workload and declared checks.".into());
    assert!(
        fixture
            .service
            .record(request.clone(), "fixture", now)
            .unwrap()
            .completed
    );
    change_run(&fixture, "history.json", "test", json!("unrelated-backup"));
    change_run(&fixture, "evidence.json", "test", json!("unrelated-backup"));
    assert!(fixture.service.record(request, "fixture", now).is_err());
}

#[test]
fn review_result_budget_is_shared_and_missing_measurements_can_only_be_inconclusive() {
    let (fixture, mut request, now) = measured_fixture();
    let prepared = fixture
        .service
        .prepare(prepare(&request.record), now)
        .unwrap();
    request
        .record
        .experiment
        .baseline
        .missing_measurements
        .extend([
            "result_usage_partial_or_unavailable".into(),
            "result_total_tokens_unavailable".into(),
            "result_usage_query_budget_exhausted".into(),
        ]);
    assert!(
        fixture
            .service
            .validate_review_evidence(&request.record, &prepared, now, Instant::now())
            .is_err()
    );
    request.record.experiment.disposition = Disposition::Inconclusive;
    fixture
        .service
        .validate_review_evidence(&request.record, &prepared, now, Instant::now())
        .unwrap();
}

#[test]
fn review_result_delivery_receipts_require_actual_timely_qualification() {
    use devcoordinator2_api::delivery::{Kind, Qualification, Receipt};
    for scenario in 0..4 {
        let (fixture, mut request, now) = measured_fixture();
        let observed = match scenario {
            1 => request.record.window_end_ms - 1,
            2 => now + 1,
            _ => now - 10_000,
        };
        let receipt = Receipt {
            receipt_id: "delivery-result".into(),
            release_id: "release-result".into(),
            repository_id: "project-alpha".into(),
            worktree_id: "w1111111111111111".into(),
            kind: Kind::Artifact,
            target: "linux-cli".into(),
            source_sha256: "a".repeat(64),
            config_sha256: "b".repeat(64),
            run_id: AFTER.into(),
            check: "focused".into(),
            artifact: "package".into(),
            artifact_sha256: "c".repeat(64),
            manifest_sha256: "d".repeat(64),
            run_metadata_sha256: "e".repeat(64),
            verification_sha256: Some("f".repeat(64)),
            qualification: if scenario == 3 {
                Qualification::PendingExternalEvidence
            } else {
                Qualification::Qualified
            },
            qualified: scenario != 3,
            verified_at_ms: Some(observed),
            checked_at_ms: now,
            delivered_at_ms: Some(observed),
            access: Some("https://example.test/download".into()),
            reason: None,
        };
        let encoded = serde_json::to_string(&receipt).unwrap();
        fixture.database.call(move |connection| {
            connection.execute_batch("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES('release-result','project-alpha',1,'Result','preview','delivered','t','fixture','t');")?;
            connection.execute("INSERT INTO release_evidence VALUES('delivery-result','release-result','project-alpha',?1)", [encoded])?;
            Ok(())
        }).unwrap();
        request
            .record
            .experiment
            .result_evidence_refs
            .push(EvidenceRef {
                kind: EvidenceKind::Release,
                reference: "delivery-result".into(),
            });
        assert_eq!(
            fixture.service.record(request, "fixture", now).is_ok(),
            scenario == 0,
            "scenario {scenario}"
        );
    }
}

#[test]
fn review_result_legacy_receipt_needs_one_validated_append_not_history_rewriting() {
    let (fixture, mut request, now) = measured_fixture();
    let mut legacy = serde_json::to_value(&request.record).unwrap();
    legacy["experiment"]
        .as_object_mut()
        .unwrap()
        .remove("comparison");
    let encoded = serde_json::to_string(&legacy).unwrap();
    let start = request.record.window_start_ms as i64;
    let end = request.record.window_end_ms as i64;
    fixture.database.call(move |connection| {
        connection.execute("INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES('legacy',1,'project-alpha',?1,?2,?3,'fixture',?4)", rusqlite::params![start,end,encoded,now as i64])?;
        Ok(())
    }).unwrap();
    assert!(
        !fixture
            .service
            .receipt(Reference {
                reference: "legacy@1".into()
            })
            .unwrap()
            .completed
    );
    request.record_id = Some("legacy".into());
    request.expected_revision = 1;
    let appended = fixture.service.record(request, "fixture", now).unwrap();
    assert!(appended.completed);
    assert_eq!(appended.revision, 2);
    assert!(
        !fixture
            .service
            .receipt(Reference {
                reference: "legacy@1".into()
            })
            .unwrap()
            .completed
    );
}
