//! The Console's token-only projection. Review evidence keeps its full timing reader.
use super::review_facts::{Facts, Token};
use super::*;

pub(super) fn read(
    connection: &Connection,
    family: &[String],
    start: u64,
    end: u64,
) -> Result<Option<Facts>, String> {
    let began = Instant::now();
    let indexed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='token_observations_repository_total_observed_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    if !indexed {
        return Ok(None);
    }
    let (_, cached) = super::review_query::selection(connection)?;
    let classification = if cached {
        "_usage_report_operations"
    } else {
        "effective_classification_events"
    };
    let provenance = if cached {
        "attribution_provenance"
    } else {
        "provenance"
    };
    let covering: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='model_requests_id_operation_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    let model_index = if covering {
        "INDEXED BY model_requests_id_operation_idx"
    } else {
        ""
    };
    let sql = format!(
        r#"WITH bounds AS (SELECT ?2 lower_ms, ?3 upper_ms), observations AS MATERIALIZED (
        SELECT source_event_id, token_count, coverage_state, model_request_id, tool_invocation_id
        FROM bounds CROSS JOIN token_observations INDEXED BY token_observations_repository_total_observed_idx
        WHERE repository_bucket IN (SELECT value FROM json_each(?1) UNION SELECT 'multi_repo' UNION SELECT 'unknown')
          AND category_path='total_tokens' AND measurement_provenance='provider_reported'
          AND observed_at_ms>=lower_ms AND observed_at_ms<upper_ms
    ), owned AS MATERIALIZED (
        SELECT token.*, COALESCE(request.operation_id, covered.operation_id, tool.operation_id) owner
        FROM observations token
        LEFT JOIN model_requests request {model_index} ON request.id=token.model_request_id
        LEFT JOIN tool_invocations tool ON tool.id=token.tool_invocation_id
        LEFT JOIN model_requests covered {model_index} ON covered.id=tool.covering_model_request_id
    ), tokens AS MATERIALIZED (
        SELECT owner,source_event_id,MAX(token_count) token_count,MAX(token_count IS NULL) unknown_count,
          MAX(coverage_state<>'complete') incomplete,
          COALESCE(MIN(token_count)<>MAX(token_count),0) OR (COUNT(token_count)>0 AND COUNT(token_count)<COUNT(*)) conflict
        FROM owned WHERE owner IN (SELECT operation_id FROM repository_attributions WHERE repository_id IN (SELECT value FROM json_each(?1)))
        GROUP BY owner,source_event_id
    )
    SELECT owner,token_count,unknown_count,incomplete,conflict FROM tokens LIMIT 200001"#
    );
    let family = serde_json::to_string(family).map_err(|_| "source_unavailable")?;
    let mut query = connection.prepare(&sql).map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "performance_prepared",
        family_count = family.len(),
        elapsed_ms = began.elapsed().as_millis()
    );
    let mut rows = query
        .query(rusqlite::params![
            family,
            i64_value(start)?,
            i64_value(end)?
        ])
        .map_err(|_| "source_unavailable")?;
    let mut facts = Facts {
        operations: BTreeMap::new(),
        tokens: Vec::new(),
        waits: HashMap::new(),
    };
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let owner: String = row.get(0).map_err(|_| "source_unavailable")?;
        let conflict: bool = row.get(4).map_err(|_| "source_unavailable")?;
        let value = row
            .get::<_, Option<i64>>(1)
            .map_err(|_| "source_unavailable")?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| "source_unavailable")?;
        facts.tokens.push(Token {
            owner,
            category: "total_tokens".into(),
            value: if conflict { None } else { value },
            incomplete: conflict
                || value.is_none()
                || row.get::<_, bool>(2).map_err(|_| "source_unavailable")?
                || row.get::<_, bool>(3).map_err(|_| "source_unavailable")?,
        });
        if facts.tokens.len() > 200_000 {
            return Err("query_too_large".into());
        }
    }
    drop(rows);
    drop(query);
    let ids = facts
        .tokens
        .iter()
        .map(|t| &t.owner)
        .collect::<BTreeSet<_>>();
    let ids = serde_json::to_string(&ids).map_err(|_| "source_unavailable")?;
    let has_index = |name: &str| -> Result<bool, String> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1)",
                [name],
                |r| r.get(0),
            )
            .map_err(|_| "source_unavailable".into())
    };
    let owner_index = if has_index("operations_review_owner_idx")? {
        "INDEXED BY operations_review_owner_idx"
    } else {
        ""
    };
    let terminal_index = if has_index("operation_events_review_terminal_idx")? {
        "INDEXED BY operation_events_review_terminal_idx"
    } else {
        ""
    };
    let context_index = if has_index("operation_work_contexts_review_idx")? {
        "INDEXED BY operation_work_contexts_review_idx"
    } else {
        ""
    };
    let attribution_index = if has_index("repository_attributions_owner_repository_idx")? {
        "INDEXED BY repository_attributions_owner_repository_idx"
    } else {
        ""
    };
    let classification_index = if cached && has_index("_usage_report_classification_owner_idx")? {
        "INDEXED BY _usage_report_classification_owner_idx"
    } else {
        ""
    };
    let metadata = format!(
        r#"    SELECT owner.id, owner.agent_id, owner.operation_kind, owner.started_at_ms,
        terminal.occurred_at_ms, terminal.event_kind, COALESCE(effective.activity_state,owner.activity_state),
        COALESCE(effective.phase,owner.phase),COALESCE(effective.activity,owner.activity),COALESCE(effective.{provenance},owner.attribution_provenance),
        NULL,owner.retry_of_operation_id IS NOT NULL,owner.rework_of_operation_id IS NOT NULL,
        context.operation_id,context.native_project_id,context.workstream_id,context.outcome_id,
        (SELECT COUNT(DISTINCT repository_id) FROM repository_attributions {attribution_index} WHERE operation_id=owner.id),terminal.duration_ns IS NOT NULL
    FROM json_each(?1) wanted CROSS JOIN operations owner {owner_index} ON owner.id=wanted.value
    LEFT JOIN {classification} effective {classification_index} ON effective.operation_id=owner.id
    LEFT JOIN operation_events terminal {terminal_index} ON terminal.operation_id=owner.id AND terminal.terminal=1
    LEFT JOIN operation_work_contexts context {context_index} ON context.operation_id=owner.id LIMIT 200001"#
    );
    let mut query = connection
        .prepare(&metadata)
        .map_err(|_| "source_unavailable")?;
    let mut rows = query.query([ids]).map_err(|_| "source_unavailable")?;
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let mut operation = super::review_facts::decode_operation(row, start, end)
            .map_err(|_| "source_unavailable")?;
        operation.interval = None;
        facts
            .operations
            .insert(operation.operation.id.clone(), operation);
    }
    tracing::debug!(
        stage = "performance_tokens",
        observations = facts.tokens.len(),
        owners = facts.operations.len(),
        elapsed_ms = began.elapsed().as_millis()
    );
    Ok(Some(facts))
}
