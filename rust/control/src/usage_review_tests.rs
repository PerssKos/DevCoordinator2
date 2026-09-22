use super::*;
use serde_json::{Value, json};

#[test]
#[ignore = "requires an explicitly authorized read-only live collector probe"]
fn live_readonly_review_probe() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .try_init();
    let input =
        std::path::PathBuf::from(std::env::var("CXM_LIVE_REVIEW_PROBE").expect("probe input"));
    let scope: Value = serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
    let source = CodexUsageSource {
        uid: rustix::process::getuid().as_raw(),
        codex_home: scope["home"].as_str().unwrap().into(),
        executable: input.clone(),
    };
    let began = Instant::now();
    let deadline = began + QUERY_TIMEOUT;
    let result = (|| {
        let connection = open_source_until(&source, deadline)?;
        connection
            .execute_batch("BEGIN")
            .map_err(|_| "source_unavailable")?;
        let canonical =
            canonical_repository(&connection, scope["repository_id"].as_str().unwrap())?;
        let family = repository_family(&connection, &canonical)?;
        let start = scope["start"].as_u64().unwrap();
        let end = scope["end"].as_u64().unwrap();
        let facts = review_facts::read(&connection, &family, start, end)?;
        let (_, groups) = review_aggregate::aggregate(&connection, facts, None, start, end)?;
        groups.finish(false)
    })();
    let evidence = match &result {
        Ok(report) => json!({"elapsed_ms":began.elapsed().as_millis(),"outcomes":report}),
        Err(reason) => json!({"elapsed_ms":began.elapsed().as_millis(),"error":reason}),
    };
    std::fs::write(
        input.with_extension("result.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(result.is_ok(), "{evidence}");
}

#[test]
#[ignore = "requires an explicitly prepared isolated collector snapshot"]
fn isolated_coordinator_daily_window_scale_probe() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .try_init();
    let directory = std::path::PathBuf::from(
        std::env::var("CXM_USAGE_SCALE_FIXTURE").expect("isolated fixture path"),
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("fixture-marker")).unwrap(),
        "Disposable isolated accounting verification copy. Never a production collector.\n"
    );
    let scopes: Value =
        serde_json::from_slice(&std::fs::read(directory.join("scope.json")).unwrap()).unwrap();
    let baseline: Value = serde_json::from_slice(
        &std::fs::read(directory.join("native-window-baseline.json")).unwrap(),
    )
    .unwrap();
    let start_ms = scopes["window_start_ms"].as_u64().unwrap();
    let end_ms = scopes["window_end_ms"].as_u64().unwrap();
    let source = CodexUsageSource {
        uid: rustix::process::getuid().as_raw(),
        codex_home: directory.clone(),
        executable: directory.join("unused"),
    };
    let mut evidence = Vec::new();
    for (index, scope) in scopes["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let began = Instant::now();
        let deadline = began + QUERY_TIMEOUT;
        let result = (|| {
            let connection = open_source_until(&source, deadline)?;
            connection
                .execute_batch("BEGIN")
                .map_err(|_| "source_unavailable")?;
            let key = canonical_repository(&connection, scope["repository_id"].as_str().unwrap())?;
            let family = repository_family(&connection, &key)?;
            let facts = review_facts::read(&connection, &family, start_ms, end_ms)?;
            let (_, groups) =
                review_aggregate::aggregate(&connection, facts, None, start_ms, end_ms)?;
            groups.finish(false)
        })();
        match result {
            Ok(report) => {
                assert_eq!(report.totals.provider_total_tokens.measured, baseline[index]["provider_tokens"].as_u64().unwrap());
                if let Some(expected) = baseline[index].get("outcomes") {
                    let mut actual = serde_json::to_value(&report).unwrap();
                    for row in actual["rows"].as_array_mut().unwrap() {
                        row.as_object_mut().unwrap().remove("title");
                    }
                    for key in ["coverage", "totals", "attributed", "unattributed", "unattributedReasons", "rows"] {
                        assert_eq!(actual[key], expected[key], "{}: {key}", scope["name"]);
                    }
                } else {
                    assert_eq!(report.attributed.operations, 0);
                }
                evidence.push(json!({"repository":scope["name"],"elapsed_ms":began.elapsed().as_millis(),
                    "operations":report.totals.operations,"provider_tokens":report.totals.provider_total_tokens.measured,"coverage":report.coverage}));
            }
            Err(reason) => evidence.push(json!({"repository":scope["name"],"elapsed_ms":began.elapsed().as_millis(),"error":reason})),
        }
    }
    std::fs::write(
        directory.join("coordinator-window-proof.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(
        evidence
            .iter()
            .all(|row| row.get("error").is_none() && row["elapsed_ms"].as_u64().unwrap() <= 15_000),
        "{evidence:?}"
    );
}

fn fixture(case: &Value) -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE operations(id TEXT,agent_id TEXT,operation_kind TEXT,started_at_ms INTEGER,
        phase TEXT,activity TEXT,activity_state TEXT,attribution_provenance TEXT,retry_of_operation_id TEXT,rework_of_operation_id TEXT);
        CREATE TABLE operation_events(operation_id TEXT,terminal INTEGER,occurred_at_ms INTEGER,event_kind TEXT,duration_ns INTEGER);
        CREATE TABLE repository_attributions(operation_id TEXT,repository_id TEXT);
        CREATE TABLE tool_invocations(id TEXT,operation_id TEXT,covering_model_request_id TEXT,execution_role TEXT,execution_group_id TEXT,operation_family TEXT);
        CREATE TABLE model_requests(id TEXT,operation_id TEXT);
        CREATE TABLE token_observations(source_event_id TEXT,model_request_id TEXT,tool_invocation_id TEXT,category_path TEXT,
            token_count INTEGER,measurement_provenance TEXT,coverage_state TEXT,observed_at_ms INTEGER);
        CREATE TABLE effective_classification_events(operation_id TEXT,phase TEXT,activity TEXT,activity_state TEXT,provenance TEXT);
        CREATE TABLE operation_work_contexts(operation_id TEXT,native_project_id TEXT,workstream_id TEXT,outcome_id TEXT);
        CREATE TABLE activity_spans(id TEXT,operation_id TEXT,activity_state TEXT,started_at_ms INTEGER);
        CREATE TABLE activity_span_events(activity_span_id TEXT,event_kind TEXT,occurred_at_ms INTEGER);
        CREATE TABLE coverage_events(operation_id TEXT,coverage_state TEXT,occurred_at_ms INTEGER);").unwrap();
    for operation in case["operations"].as_array().unwrap() {
        let id = operation["id"].as_str().unwrap();
        let start = operation["start"].as_i64().unwrap();
        let end = start + operation["duration"].as_i64().unwrap();
        connection.execute("INSERT INTO operations VALUES(?1,'agent','model_request',?2,'implementation','coding','model_active','agent_declared',NULL,NULL)", rusqlite::params![id,start]).unwrap();
        connection
            .execute(
                "INSERT INTO operation_events VALUES(?1,1,?2,?3,?4)",
                rusqlite::params![
                    id,
                    end,
                    operation["terminal"].as_str().unwrap_or("completed"),
                    (!operation["recovered"].as_bool().unwrap_or(false))
                        .then_some((end - start) * 1_000_000)
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO repository_attributions VALUES(?1,'repository')",
                [id],
            )
            .unwrap();
        connection
            .execute("INSERT INTO model_requests VALUES(?1,?1)", [id])
            .unwrap();
        if operation["snapshot"].as_bool().unwrap() {
            connection.execute("INSERT INTO operation_work_contexts VALUES(?1,'project-accounting','accounting',?2)",
                rusqlite::params![id,operation["outcome"].as_str()]).unwrap();
        }
        for category in [
            "total_tokens",
            "input_tokens",
            "input_tokens_details.cached_tokens",
        ] {
            connection.execute("INSERT INTO token_observations VALUES('event',?1,NULL,?2,?3,'provider_reported','complete',?4)",
                rusqlite::params![id,category,operation["tokens"].as_i64().unwrap(),start+20]).unwrap();
        }
    }
    if let Some(covered) = case.get("covered") {
        let start = covered["start"].as_i64().unwrap();
        let owner = covered["owner"].as_str().unwrap();
        connection.execute("INSERT INTO operations VALUES('covered','agent','hosted_tool',?1,'implementation','coding','tool_active','agent_declared',NULL,NULL)", [start]).unwrap();
        connection
            .execute(
                "INSERT INTO operation_events VALUES('covered',1,?1,'completed',0)",
                [start],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO repository_attributions VALUES('covered','repository')",
                [],
            )
            .unwrap();
        connection.execute("INSERT INTO tool_invocations VALUES('covered','covered',?1,'standalone',NULL,'test')", [owner]).unwrap();
        connection.execute("INSERT INTO operation_work_contexts SELECT 'covered',native_project_id,workstream_id,outcome_id FROM operation_work_contexts WHERE operation_id=?1", [owner]).unwrap();
        connection.execute("INSERT INTO token_observations VALUES('event',NULL,'covered','total_tokens',?1,'provider_reported','complete',?2)",
            rusqlite::params![covered["count"].as_i64().unwrap(),start]).unwrap();
    }
    if let Some(wait) = case.get("wait") {
        connection
            .execute(
                "INSERT INTO activity_spans VALUES('wait',?1,'external_wait',?2)",
                rusqlite::params![
                    wait["owner"].as_str().unwrap(),
                    wait["start"].as_i64().unwrap()
                ],
            )
            .unwrap();
        if let Some(end) = wait["end"].as_i64() {
            connection
                .execute(
                    "INSERT INTO activity_span_events VALUES('wait','ended',?1)",
                    [end],
                )
                .unwrap();
        }
    }
    connection
}

#[test]
fn outcome_reader_matches_canonical_conformance_cases() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../tests/fixtures/outcomes-v1.json")).unwrap();
    for case in fixtures["fixtures"].as_array().unwrap() {
        let connection = fixture(case);
        let start = case["window"][0].as_u64().unwrap();
        let end = case["window"][1].as_u64().unwrap();
        let facts = review_facts::read(&connection, &["repository".into()], start, end).unwrap();
        let (_, groups) =
            review_aggregate::aggregate(&connection, facts, None, start, end).unwrap();
        let report = groups.finish(false).unwrap();
        let mut rows = serde_json::to_value(report.rows).unwrap();
        for row in rows.as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("title");
        }
        assert_eq!(
            json!({"coverage": report.coverage, "totals": report.totals, "attributed": report.attributed,
            "unattributed": report.unattributed, "unattributedReasons": report.unattributed_reasons, "rows": rows}),
            case["expected"],
            "{}",
            case["name"]
        );
    }
}

#[test]
fn outcome_time_unions_are_recomputed_across_collectors() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../tests/fixtures/outcomes-v1.json")).unwrap();
    let case = &fixtures["fixtures"][0];
    let mut combined = review_aggregate::Groups::default();
    for source in 0..2 {
        let connection = fixture(case);
        let facts =
            review_facts::read(&connection, &["repository".into()], 1_000_000, 1_001_000).unwrap();
        let (_, groups) =
            review_aggregate::aggregate(&connection, facts, None, 1_000_000, 1_001_000).unwrap();
        combined.merge(groups, source).unwrap();
    }
    let report = combined.finish(false).unwrap();
    assert_eq!(
        (
            report.totals.provider_total_tokens.measured,
            report.totals.active_agent_ms.measured,
            report.totals.elapsed_execution_ms.measured,
            report.totals.recorded_wait_ms.measured
        ),
        (320, 460, 250, 20)
    );
}

#[test]
fn review_resolves_mapping_titles_and_frozen_outcome_pages_without_dashboard_warmup() {
    use crate::automation_test_support::{Fixture, START, WEEK};
    use crate::review::ReviewService;
    use devcoordinator2_api::review::Prepare;
    use std::os::unix::fs::PermissionsExt;

    let mut environment = Fixture::new();
    let fixtures: Value =
        serde_json::from_str(include_str!("../tests/fixtures/outcomes-v1.json")).unwrap();
    let source = fixture(&fixtures["fixtures"][0]);
    let key = "b".repeat(64);
    source.execute_batch("CREATE TABLE _sqlx_migrations(version INTEGER); INSERT INTO _sqlx_migrations VALUES(7);
        CREATE TABLE taxonomy_versions(version INTEGER); INSERT INTO taxonomy_versions VALUES(1);
        CREATE TABLE repositories(id TEXT); CREATE TABLE repository_merge_events(source_repository_id TEXT,target_repository_id TEXT);").unwrap();
    source
        .execute("INSERT INTO repositories VALUES(?1)", [&key])
        .unwrap();
    source
        .execute(
            "UPDATE repository_attributions SET repository_id=?1",
            [&key],
        )
        .unwrap();
    source
        .execute(
            "UPDATE operations SET agent_id='collector-agent-private'",
            [],
        )
        .unwrap();
    let home = environment.repository.join("private-collector");
    std::fs::create_dir_all(home.join("usage")).unwrap();
    let database_path = home.join("usage/usage.sqlite3");
    source
        .backup(rusqlite::MAIN_DB, &database_path, None)
        .unwrap();
    std::fs::set_permissions(&database_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let probe = home.join("identity-probe");
    let identity = json!({"schemaVersion":1,"kind":"usageRepositoryIdentity","scope":{"type":"repository","id":key},"databaseSchemaVersion":7,"taxonomyVersion":1});
    std::fs::write(
        &probe,
        format!("#!/bin/sh\nprintf x >> \"$0.calls\"\nprintf '%s\\n' '{identity}'\n"),
    )
    .unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = rustix::process::getuid().as_raw();
    environment.config.codex_usage_sources = vec![CodexUsageSource {
        uid,
        codex_home: home.clone(),
        executable: probe.clone(),
    }];
    environment.database.call(|connection| {
        for (index, id, title) in [(2, "outcome-a", "Implement accounting"), (3, "outcome-b", "Verify accounting")] {
            connection.execute("INSERT INTO tasks(task_id,repository_id,seq,position,title,outcome,kind,status,created_at,created_by,updated_at)
                VALUES(?1,'project-alpha',?2,?2,?3,?3,'improvement','in_progress','1970-01-01T00:16:40Z','fixture','1970-01-01T00:16:40Z')", rusqlite::params![id,index,title])?;
        }
        Ok(())
    }).unwrap();
    environment.service = ReviewService::new(
        environment.database.clone(),
        UsageService::with_clock(
            environment.config.clone(),
            environment.database.clone(),
            Registry::new(environment.database.clone()),
            Arc::new(crate::automation_test_support::FixtureClock),
        ),
    );
    let params = Prepare {
        repository_id: "project-alpha".into(),
        workstream_id: None,
        window_start_ms: START,
        window_end_ms: START + 1000,
        offset: 0,
        limit: 1,
        before_decision_seq: None,
        outcome_cursor: None,
        outcome_limit: Some(1),
    };
    let first = environment
        .service
        .prepare(params.clone(), START + WEEK)
        .unwrap();
    assert!(
        !first.usage.outcomes.rows.is_empty(),
        "{:?}",
        first.usage.coverage.unavailable_reasons
    );
    assert_eq!(
        first.usage.outcomes.rows[0].title.as_deref(),
        Some("Implement accounting")
    );
    assert_eq!(
        first.usage.outcomes.totals.provider_total_tokens.measured,
        160
    );
    assert_eq!(first.usage.totals.total_tokens, Some(160));
    assert_eq!(
        std::fs::read(home.join("identity-probe.calls")).unwrap(),
        b"x"
    );
    let live = Connection::open(&database_path).unwrap();
    live.execute("INSERT INTO token_observations VALUES('late','a',NULL,'total_tokens',5,'provider_reported','complete',1000999)", []).unwrap();
    let second = environment
        .service
        .prepare(
            Prepare {
                outcome_cursor: first.usage.outcomes.next_cursor.clone(),
                offset: 1,
                ..params.clone()
            },
            START + WEEK,
        )
        .unwrap();
    assert_eq!(
        second.usage.outcomes.rows[0].title.as_deref(),
        Some("Verify accounting")
    );
    assert_eq!(second.usage.totals, first.usage.totals);
    assert_eq!(second.usage.outcomes.totals, first.usage.outcomes.totals);
    assert_ne!(first.evidence, second.evidence);
    let fresh = environment
        .service
        .prepare(params.clone(), START + WEEK)
        .unwrap();
    assert_eq!(
        fresh.usage.outcomes.totals.provider_total_tokens.measured,
        165
    );
    assert_eq!(
        std::fs::read(home.join("identity-probe.calls")).unwrap(),
        b"xx"
    );
    let stale_key = "c".repeat(64);
    live.execute("INSERT INTO repositories VALUES(?1)", [&stale_key])
        .unwrap();
    environment.database.call(move |connection| {
        connection.execute("UPDATE codex_usage_repository_links SET codex_repository_id=?1 WHERE source_uid=?2", rusqlite::params![stale_key,uid])?;
        Ok(())
    }).unwrap();
    let repaired = environment
        .service
        .prepare(params.clone(), START + WEEK)
        .unwrap();
    assert_eq!(
        repaired
            .usage
            .outcomes
            .totals
            .provider_total_tokens
            .measured,
        165
    );
    assert_eq!(
        std::fs::read(home.join("identity-probe.calls")).unwrap(),
        b"xxx"
    );
    let output = serde_json::to_string(&repaired).unwrap();
    assert!(!output.contains("collector-agent-private"));
    assert!(!output.contains(&home.to_string_lossy().to_string()));
    assert!(!output.contains(&key));
    // A slow collector listed first must not consume the healthy collector's budget.
    struct DeadlineProbe {
        slow_uid: u32,
    }
    impl RepositoryProbe for DeadlineProbe {
        fn probe(
            &self,
            _source: &CodexUsageSource,
            _repository: &Path,
            _now_ms: u64,
        ) -> Result<(String, u32, u32), String> {
            Err("unexpected_unbounded_probe".into())
        }
        fn probe_until(
            &self,
            source: &CodexUsageSource,
            repository: &Path,
            now_ms: u64,
            deadline: Instant,
        ) -> Result<(String, u32, u32), String> {
            if source.uid == self.slow_uid {
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                Err("query_budget_exhausted".into())
            } else {
                HostRepositoryProbe.probe_until(source, repository, now_ms, deadline)
            }
        }
    }
    let slow_uid = if uid == 0 { 1 } else { 0 };
    let mut mixed_config = environment.config.clone();
    mixed_config.codex_usage_sources.insert(
        0,
        CodexUsageSource {
            uid: slow_uid,
            codex_home: home.clone(),
            executable: probe,
        },
    );
    let mixed = UsageService {
        registry: Registry::new(environment.database.clone()),
        usage: CodexUsage::with_probe(
            mixed_config,
            environment.database.clone(),
            Arc::new(crate::automation_test_support::FixtureClock),
            Arc::new(DeadlineProbe { slow_uid }),
        ),
    };
    let began = Instant::now();
    let partial = mixed
        .review_window(
            &RepositoryRecord {
                repository_id: "project-alpha".into(),
                display_name: "Project Alpha".into(),
                root_path: environment.repository.clone(),
            },
            None,
            START,
            START + 1000,
            began + Duration::from_secs(1),
        )
        .unwrap();
    assert!(began.elapsed() < Duration::from_secs(3));
    assert_eq!(partial.outcomes.totals.provider_total_tokens.measured, 165);
    assert_eq!(partial.outcomes.totals.provider_total_tokens.exact, None);
    assert!(
        partial
            .outcomes
            .rows
            .iter()
            .all(|row| row.effort.provider_total_tokens.exact.is_none())
    );
    assert_eq!(
        partial
            .coverage
            .unavailable_reasons
            .get("query_budget_exhausted"),
        Some(&1)
    );
    live.execute("UPDATE _sqlx_migrations SET version=8", [])
        .unwrap();
    live.execute_batch(include_str!(
        "../tests/fixtures/codex-usage-0008-activity-declarations.sql"
    ))
    .unwrap();
    let compatible = environment
        .service
        .prepare(params.clone(), START + WEEK)
        .unwrap();
    assert_eq!(
        compatible
            .usage
            .outcomes
            .totals
            .provider_total_tokens
            .measured,
        165
    );
    live.execute("UPDATE _sqlx_migrations SET version=9", [])
        .unwrap();
    let unsupported = environment.service.prepare(params, START + WEEK).unwrap();
    assert_eq!(unsupported.usage.outcomes.coverage, "unavailable");
    assert_eq!(
        unsupported
            .usage
            .outcomes
            .totals
            .provider_total_tokens
            .exact,
        None
    );
    assert!(
        unsupported
            .usage
            .coverage
            .unavailable_reasons
            .contains_key("schema_unsupported")
    );
}
