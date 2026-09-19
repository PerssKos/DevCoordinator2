use super::review_query as query;
use super::*;

const MAX_ROWS: usize = 200_000;

pub(super) struct WorkOperation {
    pub(super) operation: Operation,
    pub(super) agent_id: Option<String>,
    pub(super) interval: Option<(u64, u64)>,
    pub(super) overlaps_window: bool,
    pub(super) state: String,
    pub(super) retry: bool,
    pub(super) rework: bool,
    pub(super) key: Option<(String, Option<String>)>,
    pub(super) workstream: Option<String>,
    pub(super) reason: Option<&'static str>,
}

pub(super) struct Token {
    pub(super) owner: String,
    pub(super) category: String,
    pub(super) value: Option<u64>,
    pub(super) incomplete: bool,
}

pub(super) struct Facts {
    pub(super) operations: BTreeMap<String, WorkOperation>,
    pub(super) tokens: Vec<Token>,
    pub(super) waits: HashMap<String, (Vec<(u64, u64)>, u64)>,
}

#[tracing::instrument(skip_all)]
pub(super) fn read(
    connection: &Connection,
    family: &[String],
    start: u64,
    end: u64,
) -> Result<Facts, String> {
    let began = Instant::now();
    let (selection, cached) = query::selection(connection, query::OperationScope::Initial)?;
    let repositories = serde_json::to_string(family).map_err(|_| "source_unavailable")?;
    let params = rusqlite::params![repositories, i64_value(start)?, i64_value(end)?];
    let indexed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='operation_events_terminal_observed_idx')",
        [], |row| row.get(0),
    ).map_err(|_| "source_unavailable")?;
    // Prefer the retained time index over an expensive transient index on the
    // low-cardinality terminal flag. Older collectors may not have this index.
    let terminal_index = if indexed {
        "INDEXED BY operation_events_terminal_observed_idx"
    } else {
        ""
    };
    // The bounds must precede each fact table to enable indexed time-range seeks.
    let open_operations = if cached {
        "SELECT operation_id FROM bounds CROSS JOIN _usage_report_operations WHERE ended_at_ms IS NULL AND started_at_ms < upper_ms"
    } else {
        "SELECT operation.id FROM bounds CROSS JOIN operations operation WHERE started_at_ms < upper_ms AND NOT EXISTS (SELECT 1 FROM operation_events terminal WHERE terminal.operation_id = operation.id AND terminal.terminal = 1)"
    };
    let candidates = format!("{selection}, candidates(id) AS (
        SELECT id FROM bounds CROSS JOIN operations WHERE started_at_ms >= lower_ms AND started_at_ms < upper_ms
        UNION SELECT operation_id FROM bounds CROSS JOIN operation_events {terminal_index} WHERE terminal = 1 AND occurred_at_ms > lower_ms
        UNION {open_operations}
        UNION SELECT COALESCE(request.operation_id, covered.operation_id, tool.operation_id)
          FROM bounds CROSS JOIN token_observations token
          LEFT JOIN model_requests request ON request.id = token.model_request_id
          LEFT JOIN tool_invocations tool ON tool.id = token.tool_invocation_id
          LEFT JOIN model_requests covered ON covered.id = tool.covering_model_request_id
          WHERE token.observed_at_ms >= lower_ms AND token.observed_at_ms < upper_ms
            AND token.category_path NOT GLOB 'attribution.items.*'
        UNION SELECT operation_id FROM bounds CROSS JOIN coverage_events
          WHERE occurred_at_ms >= lower_ms AND occurred_at_ms < upper_ms)
        SELECT id FROM scoped WHERE id IN (SELECT id FROM candidates) ORDER BY id LIMIT 200001");
    let ids = connection
        .prepare(&candidates)
        .map_err(|_| "source_unavailable")?
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(|_| "source_unavailable")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "source_unavailable")?;
    if ids.len() > MAX_ROWS {
        return Err("query_too_large".into());
    }
    let ids = serde_json::to_string(&ids).map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "operation_selection",
        elapsed_ms = began.elapsed().as_millis(),
        "outcome query stage completed"
    );
    let (selection, _) = query::selection(connection, query::OperationScope::Selected)?;
    let params = rusqlite::params![repositories, i64_value(start)?, i64_value(end)?, ids];
    let mut statement = connection
        .prepare(&(selection.clone() + query::OPERATIONS))
        .map_err(|_| "source_unavailable")?;
    let mut rows = statement.query(params).map_err(|_| "source_unavailable")?;
    let mut operations = BTreeMap::new();
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let operation = decode_operation(row, start, end).map_err(|_| "source_unavailable")?;
        if operations
            .insert(operation.operation.id.clone(), operation)
            .is_some()
        {
            return Err("conflicting_operation_facts".into());
        }
        if operations.len() > MAX_ROWS {
            return Err("query_too_large".into());
        }
    }
    let mut statement = connection
        .prepare(&(selection.clone() + query::TOKENS))
        .map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "operation_facts",
        elapsed_ms = began.elapsed().as_millis(),
        "outcome query stage completed"
    );
    let mut rows = statement.query(params).map_err(|_| "source_unavailable")?;
    let mut tokens = Vec::new();
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let value = optional_unsigned(row, 2).map_err(|_| "source_unavailable")?;
        let conflict: bool = row.get(4).map_err(|_| "source_unavailable")?;
        tokens.push(Token {
            owner: row.get(0).map_err(|_| "source_unavailable")?,
            category: row.get(1).map_err(|_| "source_unavailable")?,
            value: if conflict { None } else { value },
            incomplete: conflict
                || value.is_none()
                || row.get::<_, bool>(3).map_err(|_| "source_unavailable")?,
        });
        if tokens.len() > MAX_ROWS {
            return Err("query_too_large".into());
        }
    }
    let mut statement = connection
        .prepare(&(selection + query::WAITS))
        .map_err(|_| "source_unavailable")?;
    tracing::debug!(
        stage = "token_facts",
        elapsed_ms = began.elapsed().as_millis(),
        "outcome query stage completed"
    );
    let mut rows = statement.query(params).map_err(|_| "source_unavailable")?;
    let mut waits = HashMap::<String, (Vec<(u64, u64)>, u64)>::new();
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|_| "source_unavailable")? {
        let owner: String = row.get(0).map_err(|_| "source_unavailable")?;
        let began = unsigned(row, 1).map_err(|_| "source_unavailable")?;
        let ended = optional_unsigned(row, 2).map_err(|_| "source_unavailable")?;
        let entry = waits.entry(owner).or_default();
        if let Some(ended) = ended.filter(|ended| *ended >= began) {
            entry.0.push((began.max(start), ended.min(end)));
        } else {
            entry.1 += 1;
        }
        count += 1;
        if count > MAX_ROWS {
            return Err("query_too_large".into());
        }
    }
    tracing::debug!(
        stage = "wait_facts",
        elapsed_ms = began.elapsed().as_millis(),
        "outcome query stage completed"
    );
    Ok(Facts {
        operations,
        tokens,
        waits,
    })
}

fn decode_operation(
    row: &rusqlite::Row<'_>,
    lower: u64,
    upper: u64,
) -> rusqlite::Result<WorkOperation> {
    let start = unsigned(row, 3)?;
    let end = optional_unsigned(row, 4)?;
    let project: Option<String> = row.get(14)?;
    let workstream: Option<String> = row.get(15)?;
    let outcome: Option<String> = row.get(16)?;
    for value in [&project, &workstream, &outcome].into_iter().flatten() {
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            })
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    let reason = if row.get::<_, Option<String>>(13)?.is_none() {
        Some("legacy_operation")
    } else if project.is_none() {
        Some("context_unavailable")
    } else if outcome.is_none() {
        Some("outcome_not_declared")
    } else if unsigned(row, 17)? > 1 {
        Some("multiple_repositories")
    } else {
        None
    };
    let overlaps_window =
        start < upper && end.is_none_or(|end| end > lower || end == start && start >= lower);
    let timing_known: bool = row.get(18)?;
    let interval = end
        .filter(|end| *end >= start && overlaps_window && timing_known)
        .map(|end| (start.max(lower), end.min(upper)));
    let state: String = row.get(6)?;
    let agent_id: Option<String> = row.get(1)?;
    Ok(WorkOperation {
        operation: Operation {
            id: row.get(0)?,
            kind: row.get(2)?,
            agent_id: agent_id.clone(),
            started_at_ms: start,
            finished_at_ms: end,
            phase: safe_phase(&row.get::<_, String>(7)?),
            activity: safe_label(&row.get::<_, String>(8)?),
            activity_state: state.clone(),
            provenance: safe_label(&row.get::<_, String>(9)?),
            terminal_event: row.get(5)?,
            tool_family: row.get(10)?,
        },
        agent_id,
        interval,
        overlaps_window,
        state,
        retry: row.get(11)?,
        rework: row.get(12)?,
        reason,
        key: if reason.is_none() {
            outcome.map(|outcome| (outcome, workstream.clone()))
        } else {
            None
        },
        workstream,
    })
}

fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|_| rusqlite::Error::InvalidQuery)
}
fn optional_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| rusqlite::Error::InvalidQuery)
}
