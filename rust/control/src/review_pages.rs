use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub(super) struct Snapshots(Arc<Mutex<BTreeMap<String, Snapshot>>>);

struct Snapshot {
    scope: String,
    usage: Arc<ReviewUsage>,
    created: Instant,
    bytes: usize,
}

const LIFETIME: Duration = Duration::from_secs(300);
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 16 * 1024;

impl ReviewService {
    pub(super) fn outcome_usage(
        &self,
        repository: &RepositoryRecord,
        params: &Prepare,
        deadline: Instant,
    ) -> Result<ReviewUsage, ProtocolError> {
        let limit = params.outcome_limit.unwrap_or(8);
        if !(1..=50).contains(&limit) {
            return Err(invalid("Outcome limit must be between 1 and 50"));
        }
        let scope = serde_json::to_string(&(
            &params.repository_id,
            &params.workstream_id,
            params.window_start_ms,
            params.window_end_ms,
        ))
        .map_err(|_| invalid("Cannot encode review scope"))?;
        if let Some(cursor) = &params.outcome_cursor {
            if cursor.len() > 96 {
                return Err(invalid("Invalid outcome cursor"));
            }
            let (id, offset) = cursor
                .split_once(':')
                .ok_or_else(|| invalid("Invalid outcome cursor"))?;
            let offset = offset
                .parse::<usize>()
                .map_err(|_| invalid("Invalid outcome cursor"))?;
            let snapshots = self
                .snapshots
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let snapshot = snapshots
                .get(id)
                .filter(|snapshot| snapshot.scope == scope && snapshot.created.elapsed() < LIFETIME)
                .ok_or_else(|| {
                    invalid("Outcome cursor expired or changed scope; start a new first page")
                })?;
            return page(&snapshot.usage, id, offset, limit as usize);
        }
        let mut usage = self.usage.review_window(
            repository,
            params.workstream_id.as_deref(),
            params.window_start_ms,
            params.window_end_ms,
            deadline,
        )?;
        let ids = usage
            .outcomes
            .rows
            .iter()
            .map(|row| row.outcome_id.clone())
            .collect::<Vec<_>>();
        let repository_id = params.repository_id.clone();
        let titles = self.database.call(move |connection| {
            let mut query = connection.prepare("SELECT task_id,title,kind FROM tasks WHERE repository_id = ?1 AND task_id IN (SELECT value FROM json_each(?2))")?;
            let ids = serde_json::to_string(&ids).map_err(|_| crate::database::DatabaseError::ResultType)?;
            query.query_map(rusqlite::params![repository_id, ids], |row| Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, row.get::<_, String>(2)?))))?
                .collect::<Result<BTreeMap<_, _>, _>>().map_err(Into::into)
        }).map_err(database_error)?;
        for row in &mut usage.outcomes.rows {
            row.title = titles
                .get(&row.outcome_id)
                .map(|(title, _)| title.chars().take(120).collect());
            row.kind = titles.get(&row.outcome_id).map(|(_, kind)| kind.clone());
            let entry = usage
                .outcomes
                .kinds
                .entry(row.kind.clone().unwrap_or_else(|| "unknown".into()))
                .or_default();
            let tokens = &row.effort.provider_total_tokens;
            entry.measured = entry
                .measured
                .checked_add(tokens.measured)
                .ok_or_else(|| invalid("Usage overflow"))?;
            entry.unknown += tokens.unknown;
        }
        for entry in usage.outcomes.kinds.values_mut() {
            entry.exact =
                (entry.unknown == 0 && !usage.coverage.has_gaps).then_some(entry.measured);
        }
        usage.outcomes.kinds.insert(
            "unattributed".into(),
            usage.outcomes.unattributed.provider_total_tokens.clone(),
        );
        usage.outcomes.rows.sort_by(|a, b| {
            b.effort
                .provider_total_tokens
                .measured
                .cmp(&a.effort.provider_total_tokens.measured)
                .then_with(|| a.outcome_id.cmp(&b.outcome_id))
                .then_with(|| a.workstream_id.cmp(&b.workstream_id))
        });
        let bytes = serde_json::to_vec(&usage)
            .map_err(|_| invalid("Cannot encode usage"))?
            .len();
        if bytes > MAX_SNAPSHOT_BYTES {
            return Err(invalid(
                "Outcome snapshot exceeds the supported size; narrow the window",
            ));
        }
        let created = Instant::now();
        let id: String = Sha256::digest(format!("{scope}:{created:?}").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let result = page(&usage, &id, 0, limit as usize)?;
        if result.outcomes.next_cursor.is_none() {
            return Ok(result);
        }
        let mut snapshots = self
            .snapshots
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshots.retain(|_, entry| entry.created.elapsed() < LIFETIME);
        while snapshots.len() >= 32
            || snapshots.values().map(|entry| entry.bytes).sum::<usize>() + bytes > MAX_CACHE_BYTES
        {
            if let Some(oldest) = snapshots
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(id, _)| id.clone())
            {
                snapshots.remove(&oldest);
            } else {
                break;
            }
        }
        snapshots.insert(
            id,
            Snapshot {
                scope,
                usage: Arc::new(usage),
                created,
                bytes,
            },
        );
        Ok(result)
    }
}

fn page(
    usage: &ReviewUsage,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<ReviewUsage, ProtocolError> {
    let rows = &usage.outcomes.rows;
    if offset > rows.len() || offset > 0 && offset == rows.len() {
        return Err(invalid("Outcome cursor is outside the snapshot"));
    }
    let mut result = usage.clone();
    let mut end = offset.saturating_add(limit).min(rows.len());
    loop {
        result.outcomes.rows = rows[offset..end].to_vec();
        result.outcomes.next_cursor = (end < rows.len()).then(|| format!("{id}:{end}"));
        if serde_json::to_vec(&result)
            .map_err(|_| invalid("Cannot encode usage"))?
            .len()
            <= MAX_PAGE_BYTES
        {
            return Ok(result);
        }
        if end <= offset + 1 {
            return Err(invalid(
                "Usage page exceeds the supported size; narrow the window",
            ));
        }
        end -= 1;
    }
}
