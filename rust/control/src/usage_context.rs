//! Explicit content-free operation declarations from authorized local calls.
use crate::database::{Database, DatabaseError, OptionalExtension, params};
use devcoordinator2_api::work_context::WorkAttribution;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_BINDINGS: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Binding {
    pub schema_version: u8,
    pub operation_id: String,
    pub captured_at_ms: u64,
    pub work_context: Context,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Context {
    pub schema_version: u8,
    pub id: String,
    pub repository_id: String,
    pub native_project_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub workstream_id: Option<String>,
    pub outcome_id: Option<String>,
    pub experiment_ref: Option<String>,
}

#[derive(Default)]
pub(crate) struct Window {
    pub bindings: Vec<Binding>,
    pub truncated: bool,
    pub rejected: u64,
}

fn identifier(value: &str, experiment: bool) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b) || (experiment && b == b'@'))
}

/// Identity is the kernel caller UID, never a field supplied by the client.
/// Repeated declarations are idempotent; a conflicting declaration poisons
/// that operation permanently instead of moving its effort to another outcome.
pub(crate) fn ingest(
    database: &Database,
    uid: u32,
    repository: &str,
    work: &WorkAttribution,
    at: u64,
) -> Result<(), DatabaseError> {
    let Some(work) = work.context.as_ref().filter(|_| work.diagnostic.is_none()) else {
        return Ok(());
    };
    if work.validate().is_err() {
        return Ok(());
    }
    let Some(operation) = work.operation_id.as_deref() else {
        return Ok(());
    };
    if [
        Some(operation),
        Some(repository),
        Some(work.native_project_id.as_str()),
        Some(work.thread_id.as_str()),
        work.turn_id.as_deref(),
        work.workstream_id.as_deref(),
        work.outcome_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|id| !identifier(id, false))
        || work
            .experiment_ref
            .as_deref()
            .is_some_and(|id| !identifier(id, true))
    {
        return Ok(());
    }
    let context = Context {
        schema_version: 1,
        id: String::new(),
        repository_id: repository.into(),
        native_project_id: work.native_project_id.clone(),
        thread_id: work.thread_id.clone(),
        turn_id: work.turn_id.clone(),
        workstream_id: work.workstream_id.clone(),
        outcome_id: work.outcome_id.clone(),
        experiment_ref: work.experiment_ref.clone(),
    };
    let encoded = serde_json::to_string(&context).expect("context metadata serialization");
    // Identical declarations already have durable evidence. Reading that
    // evidence preserves the first capture time and avoids an incidental
    // write for every observation of the same operation.
    let recorded = database.read(|connection| {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_usage_operation_contexts b JOIN codex_usage_contexts c ON c.context_seq=b.context_seq WHERE b.source_uid=$1 AND b.operation_id=$2 AND b.repository_id=$3 AND c.context_json=$4)",
            params![uid, operation, repository, encoded],
            |row| row.get::<_, bool>(0),
        )?)
    })?;
    if recorded {
        return Ok(());
    }
    database.try_transaction(|tx| {
        let mut context_seq = tx.run(|connection| {
            connection.query_row(
                "SELECT context_seq FROM codex_usage_contexts WHERE context_json=$1",
                [&encoded], |row| row.get::<_, i64>(0),
            ).optional().map_err(DatabaseError::from)
        })?;
        if context_seq.is_none() {
            context_seq = tx.run(|connection| {
                connection.query_row(
                    "INSERT INTO codex_usage_contexts(context_json) VALUES($1) ON CONFLICT(context_json) DO NOTHING RETURNING context_seq",
                    [&encoded], |row| row.get::<_, i64>(0),
                ).optional().map_err(DatabaseError::from)
            })?;
        }
        let context_seq = match context_seq {
            Some(sequence) => sequence,
            // Another writer may have inserted the dictionary row between
            // the read and INSERT. Read its committed identity without UPDATE.
            None => tx.run(|connection| {
                Ok(connection.query_row(
                    "SELECT context_seq FROM codex_usage_contexts WHERE context_json=$1",
                    [&encoded], |row| row.get::<_, i64>(0),
                )?)
            })?,
        };
        tx.run(|connection| Ok(connection.execute(
            "INSERT INTO codex_usage_operation_contexts(source_uid,operation_id,repository_id,captured_at_ms,context_seq) VALUES($1,$2,$3,$4,$5) ON CONFLICT(source_uid,operation_id) DO UPDATE SET rejected=codex_usage_operation_contexts.rejected OR codex_usage_operation_contexts.repository_id<>EXCLUDED.repository_id OR codex_usage_operation_contexts.context_seq<>EXCLUDED.context_seq",
            params![uid,operation,repository,at,context_seq],
        )?))?;
        Ok(())
    })?.ok_or_else(|| DatabaseError::Domain(devcoordinator2_api::ProtocolError::new(
        devcoordinator2_api::ErrorCode::Busy,
        "work context capture is unavailable while authority writers are busy",
    )))
}

pub(crate) fn read(
    database: &Database,
    uid: u32,
    repository: &str,
    start: u64,
    end: u64,
) -> Result<Window, String> {
    let rows = database.read(|connection| {
        let mut query = connection.prepare(
            "SELECT operation_id,captured_at_ms,context_json,rejected,c.context_seq FROM codex_usage_operation_contexts b JOIN codex_usage_contexts c ON c.context_seq=b.context_seq WHERE repository_id=$1 AND source_uid=$2 AND captured_at_ms >= $3 AND captured_at_ms < $4 ORDER BY captured_at_ms,operation_id LIMIT 257",
        )?;
        query.query_map(params![repository,uid,start,end], |row| {
            Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,String>(2)?,row.get::<_,bool>(3)?,row.get::<_,u64>(4)?))
        })?.collect::<Result<Vec<_>,_>>().map_err(DatabaseError::from)
    }).map_err(|_| "work_context_unavailable")?;
    let mut window = Window {
        truncated: rows.len() > MAX_BINDINGS,
        ..Default::default()
    };
    for (operation_id, captured_at_ms, encoded, rejected, sequence) in
        rows.into_iter().take(MAX_BINDINGS)
    {
        if rejected {
            window.rejected += 1;
            continue;
        }
        let mut work_context: Context =
            serde_json::from_str(&encoded).map_err(|_| "work_context_unavailable")?;
        work_context.id = format!("C{sequence}");
        window.bindings.push(Binding {
            schema_version: 1,
            operation_id,
            captured_at_ms,
            work_context,
        });
    }
    Ok(window)
}
