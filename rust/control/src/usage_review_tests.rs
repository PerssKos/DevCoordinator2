use super::*;
use serde_json::{Value, json};

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
        connection.execute_batch("ALTER TABLE token_observations ADD COLUMN repository_bucket TEXT DEFAULT 'repository'; CREATE INDEX token_observations_repository_total_observed_idx ON token_observations(repository_bucket,observed_at_ms) WHERE category_path='total_tokens' AND measurement_provenance='provider_reported';").unwrap();
        // Display reads reconcile with the exact owner/event accounting, including covered observations.
        for bucket in ["repository", "multi_repo", "unknown"] {
            connection
                .execute(
                    "UPDATE token_observations SET repository_bucket=?1",
                    [bucket],
                )
                .unwrap();
            let facts = performance::read(&connection, &["repository".into()], start, end)
                .unwrap()
                .unwrap();
            let (_, display) =
                review_aggregate::display(&connection, facts, None, start, end).unwrap();
            let display = display.finish(false).unwrap();
            assert_eq!(
                display.totals.provider_total_tokens, report.totals.provider_total_tokens,
                "display {}",
                case["name"]
            );
            assert_eq!(
                display.attributed.provider_total_tokens,
                report.attributed.provider_total_tokens
            );
            assert_eq!(
                display.unattributed.provider_total_tokens,
                report.unattributed.provider_total_tokens
            );
        }
        connection.execute("INSERT INTO token_observations(source_event_id,model_request_id,category_path,token_count,measurement_provenance,coverage_state,observed_at_ms,repository_bucket) VALUES('foreign','unowned','total_tokens',99999,'provider_reported','complete',?1,'repository')",[start as i64]).unwrap();
        let facts = performance::read(&connection, &["repository".into()], start, end)
            .unwrap()
            .unwrap();
        let (_, guarded) = review_aggregate::display(&connection, facts, None, start, end).unwrap();
        assert_eq!(
            guarded.finish(false).unwrap().totals.provider_total_tokens,
            report.totals.provider_total_tokens
        );
        assert_eq!(
            report
                .totals
                .activities
                .values()
                .map(|value| value.measured)
                .sum::<u64>(),
            report.totals.provider_total_tokens.measured
        );
        assert!(report.rows.iter().all(|row| {
            row.effort
                .activities
                .values()
                .map(|value| value.measured)
                .sum::<u64>()
                == row.effort.provider_total_tokens.measured
        }));
        let mut rows = serde_json::to_value(report.rows).unwrap();
        for row in rows.as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("title");
            row["effort"].as_object_mut().unwrap().remove("activities");
        }
        let mut actual = json!({"coverage": report.coverage, "totals": report.totals, "attributed": report.attributed, "unattributed": report.unattributed, "unattributedReasons": report.unattributed_reasons, "rows": rows});
        // Keep the shared cross-runtime v1 fixture exact; test the additive activity projection above.
        for key in ["totals", "attributed", "unattributed"] {
            actual[key].as_object_mut().unwrap().remove("activities");
        }
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}

#[test]
fn worktree_selection_keeps_owner_deduplication_and_component_totals() {
    let case = json!({
        "operations": [
            {"id":"a","start":1000,"duration":100,"outcome":"outcome-a","snapshot":true,"tokens":100},
            {"id":"b","start":1100,"duration":100,"outcome":"outcome-b","snapshot":true,"tokens":200},
            {"id":"historical","start":1200,"duration":100,"snapshot":false,"tokens":50}
        ],
        "covered":{"owner":"a","start":1030,"count":100}
    });
    let connection = fixture(&case);
    connection.execute_batch(
        "ALTER TABLE repository_attributions ADD COLUMN worktree_id TEXT;
         UPDATE repository_attributions SET worktree_id='worktree-a' WHERE operation_id='a';
         UPDATE repository_attributions SET worktree_id='worktree-b' WHERE operation_id IN ('b','covered');
         ALTER TABLE token_observations ADD COLUMN repository_bucket TEXT DEFAULT 'repository';
         CREATE INDEX token_observations_repository_total_observed_idx ON token_observations(repository_bucket,observed_at_ms) WHERE category_path='total_tokens' AND measurement_provenance='provider_reported';"
    ).unwrap();
    assert!(has_worktree_attribution(&connection).unwrap());
    for (selected, unassigned, expected) in [
        (None, true, 350),
        (Some(vec!["worktree-a".to_owned()]), false, 100),
        (
            Some(vec!["worktree-a".to_owned(), "worktree-b".to_owned()]),
            false,
            300,
        ),
        (Some(Vec::new()), true, 50),
        (Some(Vec::new()), false, 0),
    ] {
        let mut facts = performance::read(&connection, &["repository".into()], 1000, 2000)
            .unwrap()
            .unwrap();
        filter_worktree_facts(
            &connection,
            &["repository".into()],
            &mut facts,
            selected.as_deref(),
            unassigned,
        )
        .unwrap();
        for category in [
            "total_tokens",
            "input_tokens",
            "input_tokens_details.cached_tokens",
        ] {
            assert_eq!(
                facts
                    .tokens
                    .iter()
                    .filter(|token| token.category == category)
                    .filter_map(|token| token.value)
                    .sum::<u64>(),
                expected,
                "{selected:?} / unassigned={unassigned} / {category}"
            );
        }
        let (report, _) = review_aggregate::display(&connection, facts, None, 1000, 2000).unwrap();
        assert_eq!(
            report.tokens.get("total_tokens").copied().unwrap_or(0),
            expected
        );
        assert_eq!(report.activities.values().sum::<u64>(), expected);
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
