use super::*;
use crate::usage_api_fixture::{KEY, capability, response};

fn expire_completed_summaries(api: &CollectorApi) {
    // Fair-admission tests age a completed observation while keeping an
    // actively polled cold hint current. The full expire() test seam ages
    // all cache state, including abandoned hints and failed-read backoffs.
    for entry in api.0.0.lock().unwrap().entries.values_mut() {
        entry.loaded = Instant::now() - FRESH_FOR;
    }
}

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
fn source_loading_reports_unknown_estimate_overrun_and_completion_for_exact_scope() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let gate = ResponseGate::default();
    let response_gate = gate.clone();
    let peer = Peer::start(
        directory.path(),
        &directory.path().join("home"),
        move |params| {
            response_gate.wait();
            response(params, 20, 135)
        },
    );
    let release = gate.release_on_drop();
    let api = CollectorApi::default();
    let deadline = Instant::now() + Duration::from_millis(50);
    assert_eq!(
        api.summary(&peer.source, Some(KEY), None, true, 10, 20, deadline,)
            .err()
            .as_deref(),
        Some("api_refresh_pending")
    );
    let profile = profile_key(&peer.source, Some(KEY), None, true, None, &[], 10, 20).unwrap();
    let initial = api
        .loading_for(&peer.source, Some(KEY), None, true, None, &[], 10, 20)
        .unwrap();
    assert_eq!(initial.state, "reading");
    assert_eq!(initial.estimated_remaining_ms, None);
    assert_eq!(initial.estimate_samples, 0);
    {
        let mut state = api.0.0.lock().unwrap();
        state.profiles.insert(
            profile.clone(),
            ProfileHistory {
                samples: (0..3)
                    .map(|index| (Instant::now() - Duration::from_secs(index + 1), 1_000))
                    .collect(),
            },
        );
    }
    let estimated = api
        .loading_for(&peer.source, Some(KEY), None, true, None, &[], 10, 20)
        .unwrap();
    assert_eq!(estimated.estimate_samples, 3);
    assert!(estimated.estimated_remaining_ms.is_some());
    assert_eq!(
        estimated.estimate_basis.as_deref(),
        Some("recent_comparable_source_reads")
    );
    {
        let mut state = api.0.0.lock().unwrap();
        state.inflight.values_mut().next().unwrap().started =
            Instant::now() - Duration::from_secs(2);
    }
    let overrun = api
        .loading_for(&peer.source, Some(KEY), None, true, None, &[], 10, 20)
        .unwrap();
    assert_eq!(overrun.state, "estimate_exceeded");
    assert_eq!(overrun.estimated_remaining_ms, None);
    let saved: Summary = serde_json::from_value(response(
        &json!({"repositoryKey": KEY, "fromAt": 10, "toAt": 20}),
        20,
        135,
    ))
    .unwrap();
    let displayed = SummaryRead::cached_with_loading(
        Arc::new(saved),
        "api_refresh_pending",
        Some(estimated.clone()),
    )
    .unwrap()
    .source_report(Some(KEY), 1, None, true);
    assert_eq!(displayed.loading, Some(estimated));
    assert!(
        api.loading_for(&peer.source, Some(KEY), None, true, None, &[], 11, 20)
            .is_none()
    );
    release.release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    assert!(
        api.0
            .0
            .lock()
            .unwrap()
            .profiles
            .get(&profile)
            .is_some_and(|history| !history.samples.is_empty())
    );
    assert!(
        api.loading_for(&peer.source, Some(KEY), None, true, None, &[], 10, 20)
            .is_none()
    );
    assert_eq!(
        api.summary(
            &peer.source,
            Some(KEY),
            None,
            true,
            10,
            20,
            Instant::now() + Duration::from_secs(2)
        )
        .unwrap()
        .report
        .provider_tokens[0]
            .measured_tokens,
        135
    );
}

#[test]
fn loading_waiters_never_inherit_a_trained_running_estimate() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    let directory = tempfile::tempdir().unwrap();
    let gate = ResponseGate::default();
    let response_gate = gate.clone();
    let peer = Peer::start(
        directory.path(),
        &directory.path().join("home"),
        move |params| {
            response_gate.wait();
            response(params, 20, 135)
        },
    );
    let release = gate.release_on_drop();
    let api = CollectorApi::default();
    assert_eq!(
        api.summary(
            &peer.source,
            Some(KEY),
            None,
            true,
            10,
            20,
            Instant::now() + Duration::from_millis(50)
        )
        .err()
        .as_deref(),
        Some("api_refresh_pending")
    );
    assert_eq!(
        api.summary(
            &peer.source,
            Some(KEY),
            None,
            true,
            0,
            20,
            Instant::now() + Duration::from_millis(50)
        )
        .err()
        .as_deref(),
        Some("api_source_busy")
    );
    let profile = profile_key(&peer.source, Some(KEY), None, true, None, &[], 0, 20).unwrap();
    {
        let mut state = api.0.0.lock().unwrap();
        state.profiles.insert(
            profile,
            ProfileHistory {
                samples: (0..3).map(|_| (Instant::now(), 60_000)).collect(),
            },
        );
        state.cold_waiters.values_mut().next().unwrap().first_seen =
            Instant::now() - Duration::from_secs(2);
    }
    let queued = api
        .loading_for(&peer.source, Some(KEY), None, true, None, &[], 0, 20)
        .unwrap();
    release.release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    assert_eq!(queued.state, "waiting_for_source");
    assert_eq!(queued.estimated_remaining_ms, None);
    assert_eq!(queued.estimate_samples, 0);
    assert_eq!(queued.estimate_basis, None);
}

#[test]
fn loading_ignores_expired_samples_without_waiting_for_another_completion() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    let directory = tempfile::tempdir().unwrap();
    let gate = ResponseGate::default();
    let response_gate = gate.clone();
    let peer = Peer::start(
        directory.path(),
        &directory.path().join("home"),
        move |params| {
            response_gate.wait();
            response(params, 20, 135)
        },
    );
    let release = gate.release_on_drop();
    let api = CollectorApi::default();
    let _ = api.summary(
        &peer.source,
        Some(KEY),
        None,
        true,
        10,
        20,
        Instant::now() + Duration::from_millis(50),
    );
    let profile = profile_key(&peer.source, Some(KEY), None, true, None, &[], 10, 20).unwrap();
    api.0.0.lock().unwrap().profiles.insert(
        profile,
        ProfileHistory {
            samples: (0..3)
                .map(|_| {
                    (
                        Instant::now() - PROFILE_RETENTION - Duration::from_secs(1),
                        1_000,
                    )
                })
                .collect(),
        },
    );
    let loading = api
        .loading_for(&peer.source, Some(KEY), None, true, None, &[], 10, 20)
        .unwrap();
    release.release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    assert_eq!(loading.estimate_samples, 0);
    assert_eq!(loading.estimated_remaining_ms, None);
    assert_eq!(loading.estimate_basis, None);
}

#[test]
fn loading_profiles_separate_lifetime_windows_and_exact_key_revalidation() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    let directory = tempfile::tempdir().unwrap();
    let gates = [ResponseGate::default(), ResponseGate::default()];
    let response_gates = gates.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let peer = Peer::start(
        directory.path(),
        &directory.path().join("home"),
        move |params| {
            let call = observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            response_gates[call].wait();
            response(params, 20, 135)
        },
    );
    let releases = gates
        .iter()
        .map(ResponseGate::release_on_drop)
        .collect::<Vec<_>>();
    let api = CollectorApi::default();
    let _ = api.summary(
        &peer.source,
        Some(KEY),
        None,
        true,
        10,
        20,
        Instant::now() + Duration::from_millis(50),
    );
    let cold_profile = api
        .0
        .0
        .lock()
        .unwrap()
        .inflight
        .values()
        .next()
        .unwrap()
        .profile
        .clone();
    let mut releases = releases.into_iter();
    releases.next().unwrap().release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    expire_completed_summaries(&api);
    let _ = api.summary(
        &peer.source,
        Some(KEY),
        None,
        true,
        10,
        20,
        Instant::now() + Duration::from_millis(50),
    );
    let warm_profile = api
        .0
        .0
        .lock()
        .unwrap()
        .inflight
        .values()
        .next()
        .unwrap()
        .profile
        .clone();
    releases.next().unwrap().release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    assert_eq!(
        cold_profile, warm_profile,
        "the exact query scope remains stable across its revalidation"
    );
    let lifetime = profile_key(
        &peer.source,
        Some(KEY),
        None,
        true,
        None,
        &[],
        0,
        100 * 86_400_000,
    )
    .unwrap();
    let long = profile_key(
        &peer.source,
        Some(KEY),
        None,
        true,
        None,
        &[],
        1,
        100 * 86_400_000,
    )
    .unwrap();
    assert_ne!(
        lifetime, long,
        "all recorded history is a distinct query kind"
    );
}

#[test]
fn performance_loading_keeps_unknown_sources_unknown_and_clears_after_publication_pending() {
    use crate::automation_test_support::{Fixture, FixtureClock, START, WEEK};
    use crate::usage_api_fixture::{Peer, ResponseGate};
    let mut fixture = Fixture::new();
    let root = fixture._temporary.path();
    let gates = [ResponseGate::default(), ResponseGate::default()];
    let mut peers = Vec::new();
    for (index, gate) in gates.iter().enumerate() {
        let path = root.join(format!("source-{index}"));
        std::fs::create_dir(&path).unwrap();
        let gate = gate.clone();
        peers.push(Peer::start(&path, &path.join("home"), move |params| {
            gate.wait();
            let mut value = response(params, START + WEEK, 135);
            value["snapshot"]["pendingEntries"] = json!(97);
            value["snapshot"]["freshness"] = json!("stale");
            value
        }));
    }
    let releases = gates
        .iter()
        .map(ResponseGate::release_on_drop)
        .collect::<Vec<_>>();
    fixture.config.codex_usage_sources = peers.iter().map(|peer| peer.source.clone()).collect();
    let usage = UsageService::with_clock(
        fixture.config.clone(),
        fixture.database.clone(),
        Registry::new(fixture.database.clone()),
        Arc::new(FixtureClock),
    );
    let repository = RepositoryRecord {
        repository_id: "project-alpha".into(),
        display_name: "Project Alpha".into(),
        root_path: fixture.repository.clone(),
    };
    let read = || {
        usage
            .performance_window(
                &repository,
                None,
                START,
                START + WEEK,
                Instant::now() + Duration::from_millis(300),
            )
            .unwrap()
    };
    let _ = read();
    let profile = usage
        .usage
        .api
        .0
        .0
        .lock()
        .unwrap()
        .inflight
        .values()
        .find(|flight| flight.source == *peers[0].source.api_socket.as_ref().unwrap())
        .unwrap()
        .profile
        .clone();
    usage.usage.api.0.0.lock().unwrap().profiles.insert(
        profile,
        ProfileHistory {
            samples: (0..3).map(|_| (Instant::now(), 60_000)).collect(),
        },
    );
    let pending = read();
    for release in releases {
        release.release();
    }
    assert!(usage.wait_for_collector_refresh(Instant::now() + Duration::from_secs(2)));
    let loading = pending.coverage.loading.unwrap();
    assert_eq!(
        loading.estimated_remaining_ms, None,
        "one trained source cannot establish completion for an untrained sibling"
    );
    assert_eq!(loading.estimate_samples, 0);
    let complete = read();
    assert_eq!(complete.totals.total_tokens, Some(270));
    assert!(
        complete.coverage.snapshot.unwrap().refreshing,
        "publication backlog stays separate from query work"
    );
    assert_eq!(complete.coverage.loading, None);
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

#[test]
fn waiting_cold_lifetime_read_precedes_repeated_period_revalidation() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let directory = tempfile::tempdir().unwrap();
    let gate = ResponseGate::default();
    let response_gate = gate.clone();
    let revalidation = ResponseGate::default();
    let response_revalidation = revalidation.clone();
    let periods = Arc::new(AtomicUsize::new(0));
    let observed = periods.clone();
    let peer = Peer::start(
        directory.path(),
        &directory.path().join("home"),
        move |params| {
            if params["fromAt"] == 10 {
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    response_gate.wait();
                } else {
                    response_revalidation.wait();
                }
                response(params, 20, 135)
            } else {
                response(params, 20, 270)
            }
        },
    );
    let release = gate.release_on_drop();
    let release_revalidation = revalidation.release_on_drop();
    let api = CollectorApi::default();
    let read = |start, budget| {
        api.summary(
            &peer.source,
            Some(KEY),
            None,
            true,
            start,
            20,
            Instant::now() + budget,
        )
    };

    assert_eq!(
        read(10, Duration::from_millis(100)).err().as_deref(),
        Some("api_refresh_pending")
    );
    assert_eq!(
        read(0, Duration::from_millis(100)).err().as_deref(),
        Some("api_source_busy")
    );
    release.release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    expire_completed_summaries(&api);
    // Browser polls the period first. Its saved result must remain visible,
    // while its revalidation yields to the lifetime query waiting for data.
    let period = read(10, Duration::from_secs(2)).unwrap();
    assert_eq!(period.report.provider_tokens[0].measured_tokens, 135);
    let lifetime = read(0, Duration::from_secs(2));
    release_revalidation.release();
    let lifetime = lifetime.expect("a cached period must not starve a waiting lifetime read");
    assert_eq!(lifetime.report.provider_tokens[0].measured_tokens, 270);
    assert_eq!(lifetime.report.time_range.as_ref().unwrap().start_ms, 0);
    assert_eq!(periods.load(Ordering::SeqCst), 1);
}

#[test]
fn cold_waiter_hints_are_bounded_expire_and_preserve_source_isolation() {
    use crate::usage_api_fixture::{Peer, ResponseGate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let directory = tempfile::tempdir().unwrap();
    let left = directory.path().join("left");
    let right = directory.path().join("right");
    std::fs::create_dir_all(&left).unwrap();
    std::fs::create_dir_all(&right).unwrap();
    let gate = ResponseGate::default();
    let response_gate = gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let a = Peer::start(&left, &left.join("home"), move |params| {
        observed.fetch_add(1, Ordering::SeqCst);
        response_gate.wait();
        response(params, 20, 135)
    });
    let (entered_source, entered) = std::sync::mpsc::channel();
    let b = Peer::start(&right, &right.join("home"), move |params| {
        entered_source.send(()).unwrap();
        response(params, 20, 270)
    });
    let release = gate.release_on_drop();
    let api = CollectorApi::default();
    let read = |source: &crate::config::CodexUsageSource, end, budget| {
        api.summary(
            source,
            Some(KEY),
            None,
            true,
            10,
            end,
            Instant::now() + budget,
        )
    };
    assert_eq!(
        read(&a.source, 20, Duration::from_millis(100))
            .err()
            .as_deref(),
        Some("api_refresh_pending")
    );
    for end in 21..21 + MAX_CACHED as u64 + 1 {
        assert_eq!(
            read(&a.source, end, Duration::from_millis(100))
                .err()
                .as_deref(),
            Some("api_source_busy")
        );
    }
    {
        let state = api.0.0.lock().unwrap();
        assert_eq!(state.cold_waiters.len(), MAX_CACHED);
        assert!(state.pending_bytes() <= MAX_INFLIGHT_BYTES);
    }
    let independent = read(&b.source, 20, Duration::from_secs(2)).unwrap();
    assert_eq!(independent.report.provider_tokens[0].measured_tokens, 270);
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    expire_completed_summaries(&api);
    let independent = read(&b.source, 20, Duration::from_secs(2)).unwrap();
    assert_eq!(independent.report.provider_tokens[0].measured_tokens, 270);
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("another source's waiters must not block revalidation");

    release.release();
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    {
        let mut state = api.0.0.lock().unwrap();
        for waiter in state.cold_waiters.values_mut() {
            // Advance only retained waiter age, not request or source time.
            // An abandoned poll must expire without a test timer delay.
            waiter.touched = Instant::now() - COLD_WAITER_FOR;
        }
    }
    expire_completed_summaries(&api);
    let stale = read(&a.source, 20, Duration::from_secs(2)).unwrap();
    assert_eq!(stale.report.provider_tokens[0].measured_tokens, 135);
    assert!(api.wait_until_idle(Instant::now() + Duration::from_secs(2)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "expired waiters must release the revalidation turn"
    );
    assert!(api.0.0.lock().unwrap().cold_waiters.is_empty());
}

#[test]
fn usage_repository_retry_consumes_completed_query_after_clock_advances() {
    use crate::automation_test_support::{Fixture, START, WEEK};
    use crate::usage_api_fixture::{Peer, ResponseGate};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    struct MovingClock(Arc<AtomicU64>);
    impl crate::platform::Clock for MovingClock {
        fn now_utc(&self) -> time::OffsetDateTime {
            time::OffsetDateTime::from_unix_timestamp_nanos(
                i128::from(self.0.load(Ordering::SeqCst)) * 1_000_000,
            )
            .unwrap()
        }
    }
    for range in [UsageRange::Hours24, UsageRange::Days7, UsageRange::Days30] {
        let mut fixture = Fixture::new();
        let root = fixture._temporary.path();
        let first = ResponseGate::default();
        let first_response = first.clone();
        let later = ResponseGate::default();
        let later_response = later.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let peer = Peer::start(root, &root.join("usage-source"), move |params| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                first_response.wait();
            } else {
                later_response.wait();
            }
            response(params, START + WEEK, 135)
        });
        let release_first = first.release_on_drop();
        let release_later = later.release_on_drop();
        fixture.config.codex_usage_sources = vec![peer.source.clone()];
        let clock = Arc::new(AtomicU64::new(START + WEEK));
        let service = UsageService::with_clock(
            fixture.config.clone(),
            fixture.database.clone(),
            Registry::new(fixture.database.clone()),
            Arc::new(MovingClock(clock.clone())),
        );
        let params = |wait_for_refresh| devcoordinator2_api::params::UsageRepository {
            repository_id: "project-alpha".into(),
            range: range.clone(),
            worktree_ids: None,
            include_unassigned: true,
            wait_for_refresh,
        };
        let cold = service.repository(params(false)).unwrap();
        assert_eq!(cold.totals.total_tokens, None);
        assert!(
            service
                .usage
                .cache
                .wait_until_idle(Instant::now() + Duration::from_secs(3))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_first.release();
        assert!(service.wait_for_collector_refresh(Instant::now() + Duration::from_secs(2)));
        clock.store(START + WEEK + 10_000, Ordering::SeqCst);

        let refreshed = service.repository(params(true)).unwrap();
        release_later.release();
        assert_eq!(
            refreshed.totals.total_tokens,
            Some(135),
            "Usage retry must consume its completed earlier cutoff before starting a new cutoff"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a moving wall clock must not abandon the completed source query"
        );
        assert_eq!(refreshed.generated_at_ms, START + WEEK + 10_000);
        assert_eq!(
            refreshed.coverage.snapshot.as_ref().unwrap().updated_at_ms,
            Some(START + WEEK),
            "producer generation time is independent of a later report assembly"
        );
        let (start, _, bucket, count) = usage_window(&range, START + WEEK);
        let record = RepositoryRecord {
            repository_id: "project-alpha".into(),
            display_name: "Project Alpha".into(),
            root_path: fixture.repository.clone(),
        };
        let now = START + WEEK + 10_000;
        let aligned_end = now.div_ceil(bucket) * bucket;
        // Progress and chart callers share the token display entry. They must
        // consume that entry's original cutoff as well as the full detail entry.
        service
            .usage
            .repository_buckets(
                &record,
                range.clone(),
                bucket,
                count,
                now,
                aligned_end,
                true,
            )
            .unwrap();
        assert!(
            service
                .usage
                .cache
                .wait_until_idle(Instant::now() + Duration::from_secs(2))
        );
        let bucketed = service
            .usage
            .repository_buckets(
                &record,
                range.clone(),
                bucket,
                count,
                now,
                aligned_end,
                true,
            )
            .unwrap();
        assert_eq!(bucketed.totals.total_tokens, Some(135));
        assert_eq!(bucketed.generated_at_ms, now);
        assert_eq!(
            bucketed.coverage.snapshot.as_ref().unwrap().updated_at_ms,
            Some(START + WEEK)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let requests = peer.requests.lock().unwrap();
        for request in requests
            .iter()
            .filter(|request| request["method"] == "localUsage/summary")
        {
            assert_eq!(request["params"]["repositoryKey"], KEY);
            assert_eq!(request["params"]["fromAt"], start);
            assert_eq!(request["params"]["toAt"], START + WEEK);
            assert_eq!(request["params"]["includeReview"], true);
            assert_eq!(request["params"]["includeUnassigned"], true);
        }
        assert!(service.wait_for_collector_refresh(Instant::now() + Duration::from_secs(2)));
    }
}
