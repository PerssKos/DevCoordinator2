use rusqlite::Connection;

pub(super) fn selection(connection: &Connection) -> Result<(String, bool), String> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = '_usage_report_cache_meta')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "source_unavailable")?;
    let cached = exists && connection.query_row("SELECT schema_version = 1 AND ready = 1 FROM _usage_report_cache_meta WHERE singleton = 1", [], |row| row.get::<_, bool>(0)).unwrap_or(false);
    let (classification, provenance) = if cached {
        ("_usage_report_operations", "attribution_provenance")
    } else {
        ("effective_classification_events", "provenance")
    };
    Ok((
        format!(
            r#"WITH scoped AS MATERIALIZED (
        SELECT operation.* FROM operations operation
        WHERE operation.id IN (SELECT value FROM json_each(?4))
    ), bounds AS (SELECT ?2 AS lower_ms, ?3 AS upper_ms), selected AS MATERIALIZED (
        SELECT operation.*, terminal.occurred_at_ms ended_at_ms, terminal.event_kind terminal_status,
            COALESCE(effective.phase, operation.phase) effective_phase,
            COALESCE(effective.activity, operation.activity) effective_activity,
            COALESCE(effective.activity_state, operation.activity_state) effective_state,
            COALESCE(effective.{provenance}, operation.attribution_provenance) effective_provenance,
            tool.execution_role, tool.execution_group_id, tool.operation_family
        FROM scoped operation CROSS JOIN bounds
        LEFT JOIN operation_events terminal ON terminal.operation_id = operation.id AND terminal.terminal = 1
        LEFT JOIN tool_invocations tool ON tool.operation_id = operation.id
        LEFT JOIN {classification} effective ON effective.operation_id = operation.id
        WHERE operation.started_at_ms < upper_ms AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > lower_ms
            OR (terminal.occurred_at_ms = operation.started_at_ms AND operation.started_at_ms >= lower_ms))
    ), effective AS MATERIALIZED (SELECT selected.* FROM selected WHERE NOT (
        COALESCE(execution_role, 'standalone') = 'wrapper' AND execution_group_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM selected nested WHERE nested.execution_group_id = selected.execution_group_id AND nested.execution_role = 'nested')))
    {TOKEN_FACTS}"#
        ),
        cached,
    ))
}

const TOKEN_FACTS: &str = r#", owned_tokens AS MATERIALIZED (
    SELECT token.*, owner.id AS operation_id FROM scoped owner
    CROSS JOIN model_requests request ON request.operation_id = owner.id
    CROSS JOIN token_observations token ON token.model_request_id = request.id
    UNION ALL
    SELECT token.*, owner.id AS operation_id FROM scoped owner
    CROSS JOIN model_requests request ON request.operation_id = owner.id
    CROSS JOIN tool_invocations tool ON tool.covering_model_request_id = request.id
    CROSS JOIN token_observations token ON token.tool_invocation_id = tool.id
    UNION ALL
    SELECT token.*, owner.id AS operation_id FROM scoped owner
    CROSS JOIN tool_invocations tool ON tool.operation_id = owner.id AND tool.covering_model_request_id IS NULL
    CROSS JOIN token_observations token ON token.tool_invocation_id = tool.id
), token_facts AS MATERIALIZED (
    SELECT token.operation_id,
           token.source_event_id, token.category_path, token.measurement_provenance,
           MAX(token.token_count) token_count, COUNT(*) raw_count,
           MAX(token.token_count IS NULL) unknown_count,
           MAX(token.coverage_state <> 'complete') incomplete,
           (MIN(token.token_count) <> MAX(token.token_count) OR
            (COUNT(token.token_count) > 0 AND COUNT(token.token_count) < COUNT(*))) conflict
    FROM bounds CROSS JOIN owned_tokens token
    WHERE token.observed_at_ms >= lower_ms AND token.observed_at_ms < upper_ms
      AND token.category_path NOT GLOB 'attribution.items.*'
    GROUP BY token.operation_id, token.source_event_id, token.category_path, token.measurement_provenance
)"#;

pub(super) const OPERATIONS: &str = "SELECT owner.id, owner.agent_id, owner.operation_kind, owner.started_at_ms,
    terminal.occurred_at_ms ended_at_ms, terminal.event_kind, COALESCE(effective.effective_state, owner.activity_state),
    COALESCE(effective.effective_phase, owner.phase), COALESCE(effective.effective_activity, owner.activity),
    COALESCE(effective.effective_provenance, owner.attribution_provenance), effective.operation_family,
    owner.retry_of_operation_id IS NOT NULL, owner.rework_of_operation_id IS NOT NULL,
    context.operation_id, context.native_project_id, context.workstream_id, context.outcome_id,
    (SELECT COUNT(DISTINCT repository_id) FROM repository_attributions WHERE operation_id = owner.id), terminal.duration_ns IS NOT NULL
    FROM scoped owner LEFT JOIN effective ON effective.id = owner.id
    LEFT JOIN operation_events terminal ON terminal.operation_id = owner.id AND terminal.terminal = 1
    LEFT JOIN operation_work_contexts context ON context.operation_id = owner.id
    WHERE effective.id IS NOT NULL OR owner.id IN (SELECT operation_id FROM token_facts)
    ORDER BY owner.id LIMIT 200001";

pub(super) const TOKENS: &str =
    "SELECT operation_id, category_path, token_count, incomplete, COALESCE(conflict, 0)
    FROM token_facts WHERE measurement_provenance = 'provider_reported' LIMIT 200001";

pub(super) const WAITS: &str = "SELECT span.operation_id, span.started_at_ms, ended.occurred_at_ms
    FROM activity_spans span CROSS JOIN bounds
    LEFT JOIN activity_span_events ended ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
    WHERE span.operation_id IN (SELECT id FROM effective)
    AND span.activity_state IN ('user_wait', 'external_wait', 'blocked_wait')
    AND span.started_at_ms < upper_ms AND (ended.occurred_at_ms IS NULL OR ended.occurred_at_ms > lower_ms) LIMIT 200001";
