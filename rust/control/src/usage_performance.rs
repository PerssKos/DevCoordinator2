//! The Console's token-only projection. Review evidence keeps its full timing reader.
use super::review_facts::{Facts, Token};
use super::*;

pub(super) fn read(
    connection: &Connection,
    family: &[String],
    start: u64,
    end: u64,
) -> Result<Option<Facts>, String> {
    read_with_options(connection, family, start, end, true)
}

pub(super) fn read_fast(
    connection: &Connection,
    family: &[String],
    start: u64,
    end: u64,
) -> Result<Option<Facts>, String> {
    read_with_options(connection, family, start, end, false)
}

fn read_with_options(
    connection: &Connection,
    family: &[String],
    start: u64,
    end: u64,
    include_components: bool,
) -> Result<Option<Facts>, String> {
    let began = Instant::now();
    let indexed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='token_observations_repository_total_observed_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    if !indexed {
        return Ok(None);
    }
    let (_, cached) = super::review_query::selection(connection)?;
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
    let attribution_index = if connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='repository_attributions_repository_operation_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")? {
        "INDEXED BY repository_attributions_repository_operation_idx"
    } else {
        ""
    };
    let sql = format!(
        r#"WITH bounds AS (SELECT ?2 lower_ms, ?3 upper_ms), repository_owners(operation_id) AS MATERIALIZED (
        SELECT DISTINCT operation_id FROM repository_attributions {attribution_index}
        WHERE repository_id IN (SELECT value FROM json_each(?1))
    ), observations AS MATERIALIZED (
        SELECT source_event_id, token_count, coverage_state, observed_at_ms, model_request_id, tool_invocation_id
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
          MAX(observed_at_ms) observed_at_ms,
          COALESCE(MIN(token_count)<>MAX(token_count),0) OR (COUNT(token_count)>0 AND COUNT(token_count)<COUNT(*)) conflict
        FROM owned JOIN repository_owners ON repository_owners.operation_id=owned.owner
        GROUP BY owner,source_event_id
    )
    SELECT owner,token_count,unknown_count,incomplete,conflict,source_event_id,observed_at_ms FROM tokens LIMIT 200001"#
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
        rates: Vec::new(),
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
            event: row.get(5).map_err(|_| "source_unavailable")?,
            at: u64::try_from(row.get::<_, i64>(6).map_err(|_| "source_unavailable")?)
                .map_err(|_| "source_unavailable")?,
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
    tracing::debug!(
        stage = "performance_totals",
        observations = facts.tokens.len(),
        elapsed_ms = began.elapsed().as_millis()
    );
    let ids = facts
        .tokens
        .iter()
        .map(|t| &t.owner)
        .collect::<BTreeSet<_>>();
    let ids = serde_json::to_string(&ids).map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "performance_owner_ids",
        owners = facts.tokens.len(),
        elapsed_ms = began.elapsed().as_millis()
    );
    let has_index = |name: &str| -> Result<bool, String> {
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1)",
                [name],
                |r| r.get(0),
            )
            .map_err(|_| "source_unavailable".into())
    };
    let context_index = if has_index("operation_work_contexts_review_idx")? {
        "INDEXED BY operation_work_contexts_review_idx"
    } else {
        ""
    };
    // The Performance chart needs only classification and work attribution.
    // Terminal events, waits and repository-count joins remain in the full
    // review reader used for comparisons.
    let metadata = if cached {
        format!(
            r#"SELECT owner.operation_id,owner.agent_id,owner.operation_kind,owner.started_at_ms,
            NULL,NULL,owner.activity_state,owner.phase,owner.activity,owner.{provenance},NULL,0,0,
            context.operation_id,context.native_project_id,context.workstream_id,context.outcome_id,1,0
            FROM json_each(?1) wanted JOIN _usage_report_operations owner ON owner.operation_id=wanted.value
            LEFT JOIN operation_work_contexts context {context_index} ON context.operation_id=owner.operation_id LIMIT 200001"#
        )
    } else {
        format!(
            r#"SELECT owner.id,owner.agent_id,owner.operation_kind,owner.started_at_ms,
            NULL,NULL,COALESCE(effective.activity_state,owner.activity_state),
            COALESCE(effective.phase,owner.phase),COALESCE(effective.activity,owner.activity),
            COALESCE(effective.provenance,owner.attribution_provenance),NULL,0,0,
            context.operation_id,context.native_project_id,context.workstream_id,context.outcome_id,1,0
            FROM json_each(?1) wanted JOIN operations owner ON owner.id=wanted.value
            LEFT JOIN effective_classification_events effective ON effective.operation_id=owner.id
            LEFT JOIN operation_work_contexts context {context_index} ON context.operation_id=owner.id LIMIT 200001"#
        )
    };
    let mut query = connection
        .prepare(&metadata)
        .map_err(|_| "source_unavailable")?;
    let mut rows = query.query([&ids]).map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "performance_metadata_started",
        elapsed_ms = began.elapsed().as_millis()
    );
    let mut metadata_rows = 0usize;
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        metadata_rows += 1;
        if metadata_rows.is_multiple_of(5000) {
            tracing::debug!(
                stage = "performance_metadata_progress",
                rows = metadata_rows,
                elapsed_ms = began.elapsed().as_millis()
            );
        }
        let mut operation = super::review_facts::decode_operation(row, start, end)
            .map_err(|_| "source_unavailable")?;
        operation.interval = None;
        facts
            .operations
            .insert(operation.operation.id.clone(), operation);
    }
    tracing::debug!(
        stage = "performance_metadata_done",
        rows = metadata_rows,
        elapsed_ms = began.elapsed().as_millis()
    );
    tracing::debug!(
        stage = "performance_tokens",
        observations = facts.tokens.len(),
        owners = facts.operations.len(),
        elapsed_ms = began.elapsed().as_millis()
    );
    drop(rows);
    drop(query);
    if !include_components {
        return Ok(Some(facts));
    }
    super::cost::load_models(connection, &mut facts.operations)?;
    let tool_index = if has_index("token_observations_tool_source_category")? {
        "INDEXED BY token_observations_tool_source_category"
    } else {
        ""
    };
    // Keep the request-first join order. The tool branch scans its sparse
    // observation index once, rather than all tool history for every owner.
    let component_sql = format!(
        r#"WITH wanted(id) AS MATERIALIZED (SELECT value FROM json_each(?1)),
      request_ids AS MATERIALIZED (
        SELECT request.id,request.operation_id FROM wanted CROSS JOIN model_requests request ON request.operation_id=wanted.id
      )
      SELECT request.operation_id,token.category_path,token.token_count,token.coverage_state,token.source_event_id,token.observed_at_ms
      FROM request_ids request CROSS JOIN token_observations token ON token.model_request_id=request.id
      WHERE token.observed_at_ms>=?2 AND token.observed_at_ms<?3 AND token.measurement_provenance='provider_reported'
      AND token.category_path IN ('total_tokens','input_tokens','input_tokens_details.cached_tokens','input_tokens_details.cache_write_tokens','output_tokens','output_tokens_details.reasoning_tokens')
      UNION ALL
      SELECT COALESCE(request.operation_id,tool.operation_id),token.category_path,token.token_count,token.coverage_state,token.source_event_id,token.observed_at_ms
      FROM token_observations token {tool_index} CROSS JOIN tool_invocations tool ON tool.id=token.tool_invocation_id
      LEFT JOIN model_requests request ON request.id=tool.covering_model_request_id
      WHERE token.tool_invocation_id IS NOT NULL AND token.observed_at_ms>=?2 AND token.observed_at_ms<?3
      AND token.measurement_provenance='provider_reported'
      AND token.category_path IN ('total_tokens','input_tokens','input_tokens_details.cached_tokens','input_tokens_details.cache_write_tokens','output_tokens','output_tokens_details.reasoning_tokens')
      AND COALESCE(request.operation_id,tool.operation_id) IN (SELECT id FROM wanted)
      LIMIT 1200001"#
    );
    let mut query = connection
        .prepare(&component_sql)
        .map_err(|_| "source_unavailable")?;
    let mut rows = query
        .query(rusqlite::params![ids, i64_value(start)?, i64_value(end)?])
        .map_err(|_| "source_unavailable")?;
    let mut components = HashMap::<(String, String, String), Token>::new();
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        count += 1;
        if count > 1_200_000 {
            return Err("query_too_large".into());
        }
        let owner: String = row.get(0).map_err(|_| "source_unavailable")?;
        let category: String = row.get(1).map_err(|_| "source_unavailable")?;
        let value = row
            .get::<_, Option<i64>>(2)
            .map_err(|_| "source_unavailable")?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| "source_unavailable")?;
        let incomplete = row.get::<_, String>(3).map_err(|_| "source_unavailable")? != "complete"
            || value.is_none();
        let event: String = row.get(4).map_err(|_| "source_unavailable")?;
        let at = u64::try_from(row.get::<_, i64>(5).map_err(|_| "source_unavailable")?)
            .map_err(|_| "source_unavailable")?;
        let key = (owner.clone(), event.clone(), category.clone());
        match components.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(Token {
                    owner,
                    category,
                    value,
                    incomplete,
                    event,
                    at,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let previous = slot.get_mut();
                if previous.value != value {
                    previous.value = None;
                    previous.incomplete = true;
                }
                previous.incomplete |= incomplete;
                previous.at = previous.at.max(at);
            }
        }
    }
    let existing_totals = facts
        .tokens
        .iter()
        .filter(|token| token.category == "total_tokens")
        .map(|token| (token.owner.clone(), token.event.clone()))
        .collect::<std::collections::HashSet<_>>();
    facts
        .tokens
        .extend(components.into_values().filter(|token| {
            token.category != "total_tokens"
                || !existing_totals.contains(&(token.owner.clone(), token.event.clone()))
        }));
    tracing::debug!(
        stage = "performance_components",
        observations = count,
        elapsed_ms = began.elapsed().as_millis()
    );
    Ok(Some(facts))
}

/// Shared ownership and duplicate-observation rules for totals and charts.
pub(super) fn token_sql(connection: &Connection) -> Result<Option<String>, String> {
    let indexed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='token_observations_repository_total_observed_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    if !indexed {
        return Ok(None);
    }
    let covering: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='model_requests_id_operation_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    let model_index = if covering {
        "INDEXED BY model_requests_id_operation_idx"
    } else {
        ""
    };
    let owner_attribution_index: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='repository_attributions_owner_repository_idx')", [], |r| r.get(0)).map_err(|_| "source_unavailable")?;
    let attribution_index = if owner_attribution_index {
        "INDEXED BY repository_attributions_owner_repository_idx"
    } else {
        ""
    };
    Ok(Some(format!(
        r#"WITH bounds AS (SELECT ?2 lower_ms, ?3 upper_ms), observations AS MATERIALIZED (
        SELECT source_event_id, token_count, coverage_state, observed_at_ms, model_request_id, tool_invocation_id
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
          MAX(coverage_state<>'complete') incomplete, MAX(observed_at_ms) observed_at_ms,
          COALESCE(MIN(token_count)<>MAX(token_count),0) OR (COUNT(token_count)>0 AND COUNT(token_count)<COUNT(*)) conflict
        FROM owned WHERE EXISTS (SELECT 1 FROM repository_attributions {attribution_index}
          WHERE operation_id=owned.owner AND repository_id IN (SELECT value FROM json_each(?1)))
        GROUP BY owner,source_event_id
    )
    "#
    )))
}
