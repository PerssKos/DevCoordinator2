use super::*;
use crate::automation_test_support::{Fixture, START, WEEK};

fn prepare() -> Prepare {
    Prepare {
        repository_id: "project-alpha".into(),
        workstream_id: Some("spec".into()),
        window_start_ms: START,
        window_end_ms: START + WEEK,
        offset: 0,
        limit: 10,
        before_decision_seq: None,
        outcome_cursor: None,
        outcome_limit: None,
    }
}

pub(super) fn record(fixture: &Fixture) -> Record {
    let packet = fixture.service.prepare(prepare(), START + WEEK).unwrap();
    Record { record_id: None, expected_revision: 0, record: ReviewRecord {
        version: 1, repository_id: "project-alpha".into(), project_id: "project-alpha".into(), workstream_id: Some("spec".into()),
        window_start_ms: START, window_end_ms: START + WEEK, outcome_id: Some("spec".into()),
        experiment: OptimizationExperiment {
            comparison: None,
            hypothesis: "Keeping required verification avoids unsound release claims".into(),
            evidence_refs: vec![EvidenceRef { kind: EvidenceKind::Decision, reference: "decision-quality".into() }],
            alternatives: vec!["Keep intentional validation".into(), "Remove mandatory verification".into()],
            chosen_action: "Keep the agreed verification and await the owner's specification".into(),
            baseline: Baseline { evidence_refs: packet.source_refs, interpretation: "No per-task measurements exist; observed specification waiting is not agent rework".into(), missing_measurements: packet.coverage_gaps },
            success_criteria: "Keep every agreed acceptance check and preserve the current specification".into(),
            rollback_condition: "Reconsider only if evidence shows avoidable work outside required verification".into(),
            disposition: Disposition::Unchanged, result_evidence_refs: vec![], scope_repo_id: "project-alpha".into(), preserves_quality: true,
            reason: "The owner is still specifying requirements; required repeated release validation is intentional and there is no evidence of avoidable work.".into(),
            observations: vec![Observation { kind: ObservationKind::UserWait,
                interpretation: "Specification awaits owner choices; do not treat the waiting interval as wasted agent work.".into(),
                evidence_refs: vec![EvidenceRef { kind: EvidenceKind::Outcome, reference: "spec".into() }] }],
        },
    } }
}

#[test]
fn review_performance_only_week_has_no_delivery_side_effects() {
    let fixture = Fixture::new();
    let packet = fixture.service.prepare(prepare(), START + WEEK).unwrap();
    assert_eq!(
        (
            packet.window_start_ms,
            packet.window_end_ms,
            packet.usage.totals.total_tokens
        ),
        (START, START + WEEK, None)
    );
    assert!(
        packet
            .coverage_gaps
            .contains(&"total_tokens_unavailable".to_owned())
    );
    assert_eq!(packet.evidence[0].source.reference, "spec");
    let created = fixture
        .service
        .record(record(&fixture), "fixture", START + WEEK)
        .unwrap();
    assert!(created.completed);
    assert_eq!(
        fixture
            .service
            .receipt(Reference {
                reference: created.reference.clone()
            })
            .unwrap(),
        created
    );
    let counts = fixture
        .database
        .call(|connection| {
            Ok((
                connection.query_row("SELECT COUNT(*) FROM releases", [], |row| {
                    row.get::<_, i32>(0)
                })?,
                connection.query_row("SELECT COUNT(*) FROM release_evidence", [], |row| {
                    row.get::<_, i32>(0)
                })?,
            ))
        })
        .unwrap();
    assert_eq!(counts, (0, 0));
}

#[test]
fn review_requires_interpretation_and_honest_missing_measurements() {
    let fixture = Fixture::new();
    let original = record(&fixture);
    for mode in 0..5 {
        let mut invalid_record = original.clone();
        match mode {
            0 => invalid_record.record.experiment.observations.clear(),
            1 => invalid_record
                .record
                .experiment
                .baseline
                .missing_measurements
                .clear(),
            2 => invalid_record.record.experiment.scope_repo_id = "another-project".into(),
            3 => invalid_record.record.experiment.preserves_quality = false,
            4 => invalid_record.record.experiment.reason = "No change needed".into(),
            _ => unreachable!(),
        }
        assert!(
            fixture
                .service
                .record(invalid_record, "fixture", START + WEEK)
                .is_err(),
            "mode {mode}"
        );
    }
    let mut fake = original;
    fake.record.outcome_id = Some("missing-outcome".into());
    assert!(
        fixture
            .service
            .record(fake, "fixture", START + WEEK)
            .is_err()
    );
}

#[test]
fn review_coalesced_inactive_year_preserves_the_entire_receipt_window() {
    let fixture = Fixture::new();
    let end = START + 366 * 86_400_000;
    let mut input = prepare();
    input.window_end_ms = end;
    let prepared = fixture.service.prepare(input, end).unwrap();
    assert_eq!(
        (prepared.window_start_ms, prepared.window_end_ms),
        (START, end)
    );
    let mut request = record(&fixture);
    request.record.window_end_ms = end;
    request.record.experiment.baseline.evidence_refs = prepared.source_refs;
    request.record.experiment.baseline.missing_measurements = prepared.coverage_gaps;
    let revision = fixture.service.record(request, "fixture", end).unwrap();
    let receipt = fixture
        .service
        .receipt(Reference {
            reference: revision.reference,
        })
        .unwrap();
    assert!(receipt.completed);
    assert_eq!(
        (receipt.record.window_start_ms, receipt.record.window_end_ms),
        (START, end)
    );
    assert!(crate::review_validation::window(0, i64::MAX as u64).is_ok());
    assert!(crate::review_validation::window(0, i64::MAX as u64 + 1).is_err());
}

#[test]
fn review_intentional_repeated_validation_is_not_optimization_evidence() {
    let fixture = Fixture::new();
    let mut request = record(&fixture);
    request.record.experiment.observations[0].kind = ObservationKind::IntentionalValidation;
    request.record.experiment.observations[0].interpretation = "The complete release pass intentionally repeats all acceptance checks for this frozen candidate.".into();
    assert!(
        fixture
            .service
            .record(request.clone(), "fixture", START + WEEK)
            .is_ok()
    );
    request.record.experiment.disposition = Disposition::Applied;
    assert!(
        fixture
            .service
            .record(request, "fixture", START + WEEK)
            .is_err()
    );
}

#[test]
fn review_revisions_are_append_only_concurrent_and_exact() {
    let fixture = Fixture::new();
    let mut request = record(&fixture);
    let first = fixture
        .service
        .record(request.clone(), "fixture", START + WEEK)
        .unwrap();
    request.record_id = Some(first.record_id.clone());
    request.expected_revision = first.revision;
    request.record.experiment.disposition = Disposition::Inconclusive;
    let left = fixture.service.clone();
    let copy = request.clone();
    let thread = std::thread::spawn(move || left.record(copy, "fixture", START + WEEK));
    let right = fixture.service.record(request, "fixture", START + WEEK);
    assert_ne!(right.is_ok(), thread.join().unwrap().is_ok());
    assert_eq!(
        fixture
            .service
            .receipt(Reference {
                reference: first.reference.clone()
            })
            .unwrap(),
        first
    );
    let page = fixture
        .service
        .show(Show {
            repository_id: "project-alpha".into(),
            record_id: Some(first.record_id.clone()),
            offset: 0,
            limit: 1,
        })
        .unwrap();
    assert_eq!((page.records.len(), page.next_offset), (1, Some(1)));
    let latest = fixture
        .service
        .performance_reviews(devcoordinator2_api::performance::Reviews {
            repository_id: "project-alpha".into(),
            before: None,
            limit: 1,
            record_id: None,
        })
        .unwrap();
    assert_eq!(latest.total_reviews, 1);
    assert_eq!(latest.records.len(), 1);
    assert_eq!(latest.records[0].review.revision, 2);
    assert_eq!(latest.records[0].revision_count, 2);
    let revisions = fixture
        .service
        .performance_reviews(devcoordinator2_api::performance::Reviews {
            repository_id: "project-alpha".into(),
            before: None,
            limit: 1,
            record_id: Some(first.record_id.clone()),
        })
        .unwrap();
    assert_eq!(revisions.records[0].review.revision, 2);
    let older = fixture
        .service
        .performance_reviews(devcoordinator2_api::performance::Reviews {
            repository_id: "project-alpha".into(),
            before: revisions.next_before,
            limit: 1,
            record_id: Some(first.record_id),
        })
        .unwrap();
    assert_eq!(older.records[0].review.revision, 1);
    assert!(older.next_before.is_none());
    let detail = fixture
        .service
        .performance_review(
            devcoordinator2_api::performance::Review {
                repository_id: "project-alpha".into(),
                reference: first.reference,
                outcome_cursor: None,
                outcome_limit: None,
            },
            START + WEEK,
        )
        .unwrap();
    assert_eq!(detail.review.revision, 1);
    assert_eq!(
        detail.usage.outcomes.totals.provider_total_tokens.exact,
        None
    );
    assert!(detail.comparisons.is_empty());
    assert!(
        detail
            .evidence
            .iter()
            .any(|item| item.available && item.title == "Keep release proof")
    );
    assert!(
        fixture
            .database
            .call(|connection| Ok(connection.execute("DELETE FROM review_records", [])?))
            .is_err()
    );
    assert!(
        fixture
            .database
            .call(|connection| Ok(connection.execute("UPDATE review_records SET revision=9", [])?))
            .is_err()
    );
}

#[test]
fn review_input_and_page_bounds_are_enforced() {
    let fixture = Fixture::new();
    let mut request = prepare();
    request.window_end_ms = START;
    assert!(
        fixture
            .service
            .prepare(request.clone(), START + WEEK)
            .is_err()
    );
    request.window_end_ms = START + WEEK + 1;
    assert!(
        fixture
            .service
            .prepare(request.clone(), START + WEEK)
            .is_err()
    );
    request.window_end_ms = START + WEEK;
    request.limit = 11;
    assert!(fixture.service.prepare(request, START + WEEK).is_err());
    let mut request = record(&fixture);
    request.record.experiment.reason = "x".repeat(8193);
    assert!(
        fixture
            .service
            .record(request, "fixture", START + WEEK)
            .is_err()
    );
    assert!(
        fixture
            .service
            .receipt(Reference {
                reference: "nonexistent@1".into()
            })
            .is_err()
    );
}

#[test]
fn review_protocol_cli_and_mcp_preserve_exact_receipt_validation() {
    use crate::automation_test_support::FixtureClock;
    use crate::cli::{Cli, Invocation};
    use crate::control_plane::ControlPlane;
    use crate::daemon::{App, PeerCredentials};
    use clap::Parser;
    use devcoordinator2_api::{ClientContext, RequestEnvelope, ResponseEnvelope};
    use std::sync::Arc;

    let fixture = Fixture::new();
    let plane = ControlPlane::with_adapters(
        fixture.config.clone(),
        fixture.database.clone(),
        Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
        Arc::new(FixtureClock),
    )
    .unwrap();
    let app = App::with_executor(None, Arc::new(plane));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let peer = PeerCredentials {
        pid: 1,
        uid: 1000,
        gid: 1000,
    };
    let request = record(&fixture);
    let response = runtime.block_on(app.dispatch(
        RequestEnvelope {
            protocol: 2,
            id: "record".into(),
            operation: "review.record".into(),
            params: serde_json::to_value(request).unwrap(),
            client: ClientContext::default(),
        },
        peer,
    ));
    let ResponseEnvelope::Success { data, .. } = response else {
        panic!("record response: {response:?}")
    };
    let reference = data["reference"].as_str().unwrap();
    let invocation = Cli::try_parse_from([
        "devcoordinator2",
        "review",
        "show",
        reference,
        "--format",
        "json",
    ])
    .unwrap()
    .into_invocation()
    .unwrap();
    let Invocation::Remote { operation, params } = invocation else {
        panic!("expected read-only invocation")
    };
    let tool = devcoordinator2_api::mcp_tool("review_receipt").unwrap();
    assert!(tool.operation.policy.read_only());
    (tool.validate_params)(&params).unwrap();
    let response = runtime.block_on(app.dispatch(
        RequestEnvelope {
            protocol: 2,
            id: "show".into(),
            operation: operation.into(),
            params,
            client: ClientContext::default(),
        },
        peer,
    ));
    let ResponseEnvelope::Success { data: receipt, .. } = response else {
        panic!("lookup response: {response:?}")
    };
    assert_eq!(receipt, data);
    assert_eq!(receipt["record"]["repositoryId"], "project-alpha");
    assert_eq!(receipt["completed"], true);
    let response = runtime.block_on(app.dispatch(
        RequestEnvelope {
            protocol: 2,
            id: "fake".into(),
            operation: "release.evidence".into(),
            params: serde_json::json!({"reference":"invented"}),
            client: ClientContext::default(),
        },
        peer,
    ));
    assert!(matches!(response, ResponseEnvelope::Failure { .. }));
}

#[test]
fn review_prepare_exposes_retained_input_identities() {
    let fixture = Fixture::new();
    let directory = fixture.repository.join(".devcoordinator/test");
    std::fs::create_dir_all(&directory).unwrap();
    let run = "t20260903T120718Z-57067c";
    std::fs::write(directory.join("history.json"), serde_json::to_vec(&serde_json::json!({"schema":2,"runs":[{
        "run_id":run,"test":"focused","status":"passed","started_at":"1970-01-02T00:00:00Z","finished_at":"1970-01-02T00:01:00Z","duration_seconds":60.0,"exit_code":0
    }]})).unwrap()).unwrap();
    std::fs::write(directory.join("evidence.json"), serde_json::to_vec(&serde_json::json!({"schema":2,"runs":[{
        "run_id":run,"test":"focused","proof":"complete","status":"passed","source_digest":"a".repeat(64),"config_digest":"b".repeat(64),"requested_tier":"release","readiness_eligible":true,"selection":[],"checks":[]
    }]})).unwrap()).unwrap();
    let packet = fixture.service.prepare(prepare(), START + WEEK).unwrap();
    let evidence = packet
        .evidence
        .iter()
        .find(|evidence| evidence.source.kind == EvidenceKind::Run)
        .unwrap();
    assert_eq!(
        (
            evidence.source_sha256.as_deref(),
            evidence.config_sha256.as_deref()
        ),
        (Some("a".repeat(64).as_str()), Some("b".repeat(64).as_str()))
    );
    assert!(
        !packet
            .coverage_gaps
            .contains(&"run_source_identity_unavailable".into())
    );
}

#[test]
fn work_context_review_packet_exposes_native_links_without_allocating_repository_totals() {
    use devcoordinator2_api::work_context::{WorkContext, WorkSource};
    let fixture = Fixture::new();
    let directory = fixture.repository.join(".devcoordinator/test");
    std::fs::create_dir_all(&directory).unwrap();
    let context = devcoordinator2_api::ClientContext {
        work: Some(WorkContext::parse(r#"{"version":1,"native_project_id":"native-clock-hash","thread_id":"thread","turn_id":"turn","operation_id":"operation","workstream_id":"spec","outcome_id":"spec","experiment_ref":"review-1@1"}"#).unwrap()),
        work_source: Some(WorkSource::Environment), ..Default::default()
    }.work_attribution();
    std::fs::write(directory.join("history.json"), serde_json::to_vec(&serde_json::json!({"schema":2,"runs":[{
        "run_id":"t20260903T120718Z-57067c","test":"focused","status":"passed","started_at":"1970-01-02T00:00:00Z","finished_at":"1970-01-02T00:01:00Z","duration_seconds":60.0,"exit_code":0,"work":context
    }]})).unwrap()).unwrap();
    let packet = fixture.service.prepare(prepare(), START + WEEK).unwrap();
    assert_eq!(packet.repository_id, "project-alpha");
    let run = packet
        .evidence
        .iter()
        .find(|evidence| evidence.source.kind == EvidenceKind::Run)
        .unwrap();
    assert_eq!(run.work, context);
    assert_eq!(packet.usage.totals.total_tokens, None);
    assert!(
        packet
            .coverage_gaps
            .contains(&"task_usage_attribution_unavailable".into())
    );
    assert!(
        !packet
            .coverage_gaps
            .contains(&"run_work_context_unavailable".into())
    );
}

#[test]
fn review_schedule_routes_escalates_and_only_valid_receipts_advance_it() {
    use devcoordinator2_api::review_policy::*;
    let fixture = Fixture::new();
    let events = crate::events::EventService::new(fixture.database.clone()).unwrap();
    fixture
        .service
        .review_reminders(&events, START + WEEK)
        .unwrap();
    let count = || {
        fixture
            .database
            .call(|c| {
                Ok(
                    c.query_row("SELECT COUNT(*) FROM review_reminders", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .unwrap()
    };
    assert_eq!(count(), 0);
    let scope = Scope {
        repository_id: "project-alpha".into(),
        workstream_id: Some("spec".into()),
    };
    let policy = fixture
        .service
        .policy_set(
            Set {
                repository_id: scope.repository_id.clone(),
                workstream_id: scope.workstream_id.clone(),
                review_interval_ms: None,
                escalation_interval_ms: None,
                active: true,
                window_start_ms: Some(START),
                clock_action: None,
            },
            START,
        )
        .unwrap();
    assert_eq!(
        (policy.review_interval_ms, policy.escalation_interval_ms),
        (86_400_000, 3_600_000)
    );
    let due = policy.window_end_ms;
    fixture
        .service
        .delivery_register(
            Register {
                repository_id: scope.repository_id.clone(),
                workstream_id: scope.workstream_id.clone(),
                owner_thread_id: "thread-1".into(),
                mode: DeliveryMode::CodexAlarm,
                alarm_namespace: "codex.review.v1".into(),
                capability_revision: 1,
                lease_expires_at: due + 3_600_000,
            },
            due,
            RegistrationMode::Replace,
        )
        .unwrap();
    fixture.service.review_reminders(&events, due).unwrap();
    fixture.service.review_reminders(&events, due).unwrap();
    assert_eq!(count(), 1);
    let native = fixture
        .service
        .delivery_pending(
            PendingRequest {
                alarm_namespace: "codex.review.v1".into(),
                after_id: 0,
            },
            due,
        )
        .unwrap();
    assert_eq!(native.reminders.len(), 1);
    assert_eq!(native.reminders[0].owner_thread_id, "thread-1");
    assert!(native.reminders[0].last_completed_receipt.is_none());
    let message_count = || {
        fixture
            .database
            .call(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM agent_messages", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .unwrap()
    };
    assert_eq!(message_count(), 0);
    // Restart the owner of the event service; the policy/outbox still deduplicate.
    drop(events);
    let events = crate::events::EventService::new(fixture.database.clone()).unwrap();
    fixture
        .service
        .review_reminders(&events, due + 3_600_000)
        .unwrap();
    fixture
        .service
        .review_reminders(&events, due + 3_600_001)
        .unwrap();
    assert_eq!((count(), message_count()), (2, 2));
    assert!(
        fixture
            .service
            .policy_status(scope.clone(), due + 3_600_001)
            .unwrap()
            .unwrap()
            .escalated
    );
    // Transport acknowledgements do not satisfy a review obligation.
    fixture
        .database
        .call(|c| {
            c.execute(
                "UPDATE agent_messages SET acknowledged_at='1970-01-03T00:00:00Z'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        fixture
            .service
            .policy_status(scope.clone(), due + 3_600_001)
            .unwrap()
            .unwrap()
            .due
    );
    let receipt = fixture
        .service
        .record(record(&fixture), "fixture", START + WEEK)
        .unwrap();
    assert!(receipt.completed);
    let after = fixture
        .service
        .policy_status(scope, START + WEEK)
        .unwrap()
        .unwrap();
    assert!(!after.due);
    assert_eq!(after.last_completed_receipt, Some(receipt.reference));
    assert_eq!(after.window_start_ms, START + WEEK);
    fixture
        .service
        .review_reminders(&events, START + WEEK)
        .unwrap();
    assert_eq!(count(), 2);
}

#[test]
fn review_policy_without_registration_uses_messages_and_preserves_window_on_update() {
    use devcoordinator2_api::review_policy::*;
    let fixture = Fixture::new();
    let events = crate::events::EventService::new(fixture.database.clone()).unwrap();
    let set = Set {
        repository_id: "project-alpha".into(),
        workstream_id: None,
        review_interval_ms: None,
        escalation_interval_ms: None,
        active: true,
        window_start_ms: None,
        clock_action: None,
    };
    let first = fixture.service.policy_set(set.clone(), START).unwrap();
    let renewed = fixture
        .service
        .policy_set(
            Set {
                clock_action: Some(ClockAction::FollowExisting),
                ..set
            },
            START + 1000,
        )
        .unwrap();
    assert_eq!(first.window_start_ms, renewed.window_start_ms);
    fixture
        .service
        .review_reminders(&events, first.window_end_ms)
        .unwrap();
    let counts = fixture
        .database
        .call(|c| {
            Ok((
                c.query_row(
                    "SELECT COUNT(*) FROM agent_messages WHERE kind='performance_review.reminder'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
                c.query_row(
                    "SELECT COUNT(*) FROM owned_events WHERE kind='review.reminder'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(counts, (1, 0));
    let receipt = fixture
        .service
        .record(record(&fixture), "fixture", START + WEEK)
        .unwrap();
    let resumed = fixture
        .service
        .policy_set(
            Set {
                repository_id: "project-alpha".into(),
                workstream_id: Some("spec".into()),
                review_interval_ms: None,
                escalation_interval_ms: None,
                active: true,
                window_start_ms: Some(START),
                clock_action: None,
            },
            START + WEEK + 1000,
        )
        .unwrap();
    assert_eq!(resumed.window_start_ms, START + WEEK);
    assert_eq!(resumed.last_completed_receipt, Some(receipt.reference));
}

#[test]
fn review_policy_requires_explicit_clock_choice_for_legacy_existing_scope() {
    use devcoordinator2_api::review_policy::*;
    let fixture = Fixture::new();
    fixture
        .database
        .call(|c| {
            c.execute(
                "INSERT INTO review_policies(repository_id,workstream_key,interval_ms,escalation_ms,active,window_start_ms,window_end_ms,last_receipt) VALUES('project-alpha','\"legacy\"',86400000,3600000,1,1000000,87400000,NULL)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let status = fixture
        .service
        .policy_status(
            Scope {
                repository_id: "project-alpha".into(),
                workstream_id: Some("legacy".into()),
            },
            START,
        )
        .unwrap()
        .unwrap();
    assert!(status.choice_required);
    assert_eq!(status.clock_state, "legacy-only");
    assert!(
        status
            .choice_explanation
            .as_deref()
            .is_some_and(|value| value.contains("existing review clock"))
    );
    let error = fixture
        .service
        .policy_set(
            Set {
                repository_id: "project-alpha".into(),
                workstream_id: Some("legacy".into()),
                review_interval_ms: None,
                escalation_interval_ms: None,
                active: true,
                window_start_ms: None,
                clock_action: None,
            },
            START,
        )
        .unwrap_err();
    assert!(error.message.contains("clock_choice_required"));
    let retired = fixture
        .service
        .policy_set(
            Set {
                repository_id: "project-alpha".into(),
                workstream_id: Some("legacy".into()),
                review_interval_ms: None,
                escalation_interval_ms: None,
                active: true,
                window_start_ms: None,
                clock_action: Some(ClockAction::RetireExisting),
            },
            START,
        )
        .unwrap();
    assert!(!retired.active);
    assert_eq!(retired.clock_state, "retired");
    assert!(!retired.choice_required);
    assert_eq!(retired.clock_start_ms, 1_000_000);
    assert_eq!(retired.clock_due_at_ms, 87_400_000);
    assert_eq!(retired.clock_hard_stop_at_ms, 91_000_000);
}

#[test]
fn retired_legacy_clock_does_not_require_a_new_choice() {
    use devcoordinator2_api::review_policy::*;
    let fixture = Fixture::new();
    fixture
        .database
        .call(|c| {
            c.execute(
                "INSERT INTO review_policies(repository_id,workstream_key,interval_ms,escalation_ms,active,window_start_ms,window_end_ms,last_receipt) VALUES('project-alpha','\"retired-legacy\"',86400000,3600000,0,1000000,87400000,NULL)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let status = fixture
        .service
        .policy_status(
            Scope {
                repository_id: "project-alpha".into(),
                workstream_id: Some("retired-legacy".into()),
            },
            START,
        )
        .unwrap()
        .unwrap();
    assert!(!status.active);
    assert_eq!(status.clock_state, "retired");
    assert!(!status.choice_required);
    assert!(status.choice_explanation.is_none());
}
