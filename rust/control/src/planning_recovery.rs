//! Additive planning recovery, serialized by the existing database owner.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use devcoordinator2_api::recovery::{Mapping, Receipt, Request};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::database::{Database, DatabaseError};
use crate::planning_backup::{InspectRequest, Inspection};

type Row = BTreeMap<String, Value>;
type Tables = BTreeMap<String, Vec<Row>>;
const TABLES: &[(&str, &str)] = &[
    ("releases", "release_id"),
    ("tasks", "task_id"),
    ("decisions", "decision_id"),
    ("plan_events", "event_id"),
    ("release_evidence", "receipt_id"),
    ("decision_summaries", "covers_through_seq"),
    ("visual_feedback", "feedback_id"),
    ("visual_feedback_comments", "comment_id"),
    ("visual_feedback_events", "event_id"),
];

fn invalid(message: &str) -> DatabaseError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message).into()
}

pub fn recover(
    database: &Database,
    request: Request,
    actor: &str,
    now: &str,
) -> Result<Receipt, ProtocolError> {
    let inspection = InspectRequest {
        transaction_dir: PathBuf::from(&request.transaction_dir),
        repository_id: request.repository_id.clone(),
        task_ids: vec![],
        include_identities: true,
    };
    let (snapshot, source) = crate::planning_backup::inspect_with(&inspection, |connection| {
        read_tables(connection, &request.repository_id)
            .map_err(|_| "cannot read the repository planning rows".to_owned())
    })
    .map_err(|message| ProtocolError::new(ErrorCode::ParamsInvalid, message))?;
    if snapshot.backup_sha256 != request.backup_sha256 {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "backup does not match the selected recovery hash",
        ));
    }
    if !snapshot.repository_present {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "the selected repository is absent from this backup",
        ));
    }
    let actor = actor.to_owned();
    let now = now.to_owned();
    database
        .transaction(move |transaction| {
            // inspect_with verifies the source hash and file identity both before
            // and after capturing these owned rows. Import only that verified
            // in-memory snapshot; do not re-read gigabytes while holding the
            // shared database actor. The live fingerprint is checked atomically.
            apply(transaction, &request, &snapshot, &source, &actor, &now)
        })
        .map_err(|error| match error {
            DatabaseError::Domain(error) => error,
            _ => ProtocolError::new(
                ErrorCode::InternalError,
                "planning recovery failed; no partial import was committed",
            ),
        })
}

fn read_tables(connection: &Connection, repository_id: &str) -> Result<Tables, DatabaseError> {
    let mut result = Tables::new();
    for &(table, key) in TABLES {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !exists {
            if matches!(
                table,
                "release_evidence"
                    | "decision_summaries"
                    | "visual_feedback"
                    | "visual_feedback_comments"
                    | "visual_feedback_events"
            ) {
                result.insert(table.to_owned(), vec![]);
                continue;
            }
            return Err(invalid("the backup is missing a required planning table"));
        }
        let scope = if matches!(table, "visual_feedback_comments" | "visual_feedback_events") {
            "feedback_id IN (SELECT feedback_id FROM visual_feedback WHERE repository_id=?1)"
        } else {
            "repository_id=?1"
        };
        let mut statement = connection.prepare(&format!(
            "SELECT * FROM {table} WHERE {scope} ORDER BY {key}"
        ))?;
        let columns: Vec<String> = statement
            .column_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut rows = statement.query([repository_id])?;
        let mut saved = vec![];
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let mut values = Row::new();
            for (index, name) in columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(value) => Value::from(value),
                    rusqlite::types::ValueRef::Text(value) => {
                        bytes = bytes.saturating_add(value.len());
                        Value::String(
                            std::str::from_utf8(value)
                                .map_err(|_| invalid("planning text is not UTF-8"))?
                                .to_owned(),
                        )
                    }
                    _ => return Err(invalid("unsupported planning storage value")),
                };
                values.insert(name.clone(), value);
            }
            saved.push(values);
            if saved.len() > 100_000 || bytes > 64 * 1024 * 1024 {
                return Err(invalid(
                    "repository planning history exceeds the bounded recovery limit",
                ));
            }
        }
        result.insert(table.to_owned(), saved);
    }
    Ok(result)
}

fn text<'a>(row: &'a Row, field: &str) -> Result<&'a str, DatabaseError> {
    row.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("planning record is missing a required text field"))
}

fn number(row: &Row, field: &str) -> Result<i64, DatabaseError> {
    row.get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid("planning record is missing a required integer field"))
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fingerprint(connection: &Connection, repository_id: &str) -> Result<String, DatabaseError> {
    let tables = read_tables(connection, repository_id)?;
    let repository: (String, Option<String>, Option<String>) = connection.query_row(
        "SELECT root_path,archived_at,merged_into_repository_id FROM repositories WHERE repository_id=?1",
        [repository_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    )?;
    let rows = serde_json::to_vec(&(repository, tables))
        .map_err(|_| invalid("cannot fingerprint planning state"))?;
    Ok(digest(rows))
}

fn verify_reference(
    connection: &Connection,
    tables: &Tables,
    table: &str,
    key: &str,
    id: &str,
    repository: &str,
) -> Result<(), DatabaseError> {
    if tables[table]
        .iter()
        .any(|r| r.get(key).and_then(Value::as_str) == Some(id))
    {
        return Ok(());
    }
    let owner: Option<String> = connection
        .query_row(
            &format!("SELECT repository_id FROM {table} WHERE {key}=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    if owner.as_deref() == Some(repository) {
        Ok(())
    } else {
        Err(invalid(
            "a recovered relationship is missing or belongs to another repository",
        ))
    }
}

fn validate(
    connection: &Connection,
    source: &Tables,
    repository: &str,
) -> Result<(), DatabaseError> {
    let mut parents = BTreeMap::new();
    for &(table, key) in TABLES {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = statement
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        for row in &source[table] {
            if (row.contains_key("repository_id") && text(row, "repository_id")? != repository)
                || row.keys().any(|k| !columns.contains(k))
            {
                return Err(invalid(
                    "planning schema or repository ownership does not match",
                ));
            }
            if matches!(
                table,
                "plan_events" | "decision_summaries" | "visual_feedback_events"
            ) {
                continue;
            }
            let id = text(row, key)?;
            let exists: bool = connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {key}=?1)"),
                [id],
                |r| r.get(0),
            )?;
            if exists {
                return Err(invalid(
                    "a saved record identity already exists; recovery never overwrites live records",
                ));
            }
        }
    }
    for row in &source["tasks"] {
        let id = text(row, "task_id")?;
        let parent = row.get("parent_task_id").and_then(Value::as_str);
        if let Some(parent) = parent {
            verify_reference(connection, source, "tasks", "task_id", parent, repository)?;
        }
        if let Some(release) = row.get("release_id").and_then(Value::as_str) {
            verify_reference(
                connection,
                source,
                "releases",
                "release_id",
                release,
                repository,
            )?;
        }
        parents.insert(id, parent);
    }
    for &id in parents.keys() {
        let mut visiting = BTreeSet::new();
        let mut cursor = Some(id);
        while let Some(node) = cursor {
            if !visiting.insert(node) {
                return Err(invalid("recovered tasks contain a parent cycle"));
            }
            cursor = parents.get(node).copied().flatten();
        }
    }
    for row in &source["decisions"] {
        if let Some(id) = row.get("superseded_by").and_then(Value::as_str) {
            verify_reference(
                connection,
                source,
                "decisions",
                "decision_id",
                id,
                repository,
            )?;
        }
        if let Some(reference) = row.get("ref").and_then(Value::as_str) {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM decisions WHERE repository_id=?1 AND ref=?2)",
                params![repository, reference],
                |r| r.get(0),
            )?;
            if exists {
                return Err(invalid(
                    "a saved decision reference already exists; reconcile it before recovery",
                ));
            }
        }
    }
    for row in &source["visual_feedback"] {
        verify_reference(
            connection,
            source,
            "tasks",
            "task_id",
            text(row, "task_id")?,
            repository,
        )?;
    }
    for row in source["visual_feedback_comments"]
        .iter()
        .chain(&source["visual_feedback_events"])
    {
        verify_reference(
            connection,
            source,
            "visual_feedback",
            "feedback_id",
            text(row, "feedback_id")?,
            repository,
        )?;
    }
    for row in &source["release_evidence"] {
        verify_reference(
            connection,
            source,
            "releases",
            "release_id",
            text(row, "release_id")?,
            repository,
        )?;
    }
    Ok(())
}

fn insert_row(connection: &Connection, table: &str, row: &Row) -> Result<(), DatabaseError> {
    let columns = row
        .keys()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",");
    let placeholders = std::iter::repeat_n("?", row.len())
        .collect::<Vec<_>>()
        .join(",");
    let values = row
        .values()
        .map(|value| match value {
            Value::Null => Ok(rusqlite::types::Value::Null),
            Value::String(s) => Ok(rusqlite::types::Value::Text(s.clone())),
            Value::Number(n) => n
                .as_i64()
                .map(rusqlite::types::Value::Integer)
                .ok_or_else(|| invalid("non-integer planning value")),
            _ => Err(invalid("unsupported planning value")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    connection.execute(
        &format!("INSERT INTO {table} ({columns}) VALUES ({placeholders})"),
        params_from_iter(values),
    )?;
    Ok(())
}

fn apply(
    connection: &Connection,
    request: &Request,
    snapshot: &Inspection,
    source: &Tables,
    actor: &str,
    now: &str,
) -> Result<Receipt, DatabaseError> {
    let existing: Option<String> = connection.query_row(
        "SELECT receipt_json FROM planning_recoveries WHERE repository_id=?1 AND backup_sha256=?2",
        params![request.repository_id, snapshot.backup_sha256], |r| r.get(0),
    ).optional()?;
    if let Some(existing) = existing {
        let mut receipt: Receipt = serde_json::from_str(&existing)
            .map_err(|_| invalid("invalid saved recovery receipt"))?;
        for mapping in &receipt.mappings {
            let key = match mapping.kind.as_str() {
                "tasks" => "task_id",
                "decisions" => "decision_id",
                "releases" => "release_id",
                _ => continue,
            };
            let owner: Option<String> = connection
                .query_row(
                    &format!("SELECT repository_id FROM {} WHERE {key}=?1", mapping.kind),
                    [&mapping.id],
                    |r| r.get(0),
                )
                .optional()?;
            if owner.as_deref() != Some(&request.repository_id) {
                return Err(invalid(
                    "a previously recovered identity is missing; inspect the recovery history",
                ));
            }
        }
        receipt.status = "already_applied".to_owned();
        return Ok(receipt);
    }
    let present: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE repository_id=?1)",
        [&request.repository_id],
        |r| r.get(0),
    )?;
    if !present {
        return Err(invalid(
            "the recovery target is not a registered repository",
        ));
    }
    validate(connection, source, &request.repository_id)?;
    let live_sha256 = fingerprint(connection, &request.repository_id)?;
    if request.apply && request.expected_live_sha256.as_deref() != Some(&live_sha256) {
        return Err(invalid(
            "live planning state changed or no dry-run fingerprint was supplied; prepare recovery again",
        ));
    }
    let recovery_id = format!(
        "pr{}",
        &digest(format!(
            "{}:{}",
            request.repository_id, snapshot.backup_sha256
        ))[..30]
    );
    let mut mappings = vec![];
    let mut prepared = source.clone();
    for &(table, key) in &TABLES[..3] {
        let mut next: i64 = connection.query_row(
            &format!("SELECT COALESCE(MAX(seq),0) FROM {table} WHERE repository_id=?1"),
            [&request.repository_id],
            |r| r.get(0),
        )?;
        let rows = prepared.get_mut(table).expect("known table");
        rows.sort_by_key(|r| r.get("seq").and_then(Value::as_i64).unwrap_or(0));
        for row in rows {
            let original_sequence = number(row, "seq")?;
            next = next
                .checked_add(1)
                .filter(|n| *n <= i64::from(u32::MAX))
                .ok_or_else(|| invalid("planning sequence exhausted"))?;
            mappings.push(Mapping {
                kind: table.to_owned(),
                id: text(row, key)?.to_owned(),
                original_sequence,
                assigned_sequence: next,
            });
            row.insert("seq".to_owned(), Value::from(next));
        }
    }
    let receipt = Receipt {
        recovery_id: recovery_id.clone(),
        repository_id: request.repository_id.clone(),
        backup_sha256: snapshot.backup_sha256.clone(),
        live_sha256,
        provenance: snapshot.provenance.to_owned(),
        status: if request.apply { "applied" } else { "prepared" }.to_owned(),
        counts: source
            .iter()
            .map(|(k, v)| (k.clone(), v.len() as u64))
            .collect(),
        mappings,
    };
    if !request.apply {
        return Ok(receipt);
    }
    connection.execute_batch("PRAGMA defer_foreign_keys=ON;")?;
    connection.execute(
        "INSERT INTO planning_recoveries(recovery_id,repository_id,backup_sha256,before_sha256,at,actor,receipt_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![recovery_id, request.repository_id, snapshot.backup_sha256, receipt.live_sha256, now, actor, serde_json::to_string(&receipt).map_err(|_| invalid("cannot encode recovery receipt"))?],
    )?;
    for &(table, key) in TABLES {
        for original in &source[table] {
            let mut row = original.clone();
            let id = if matches!(
                table,
                "plan_events" | "decision_summaries" | "visual_feedback_events"
            ) {
                number(&row, key)?.to_string()
            } else {
                text(&row, key)?.to_owned()
            };
            let original_sequence = original.get("seq").and_then(Value::as_i64);
            let mapping = receipt
                .mappings
                .iter()
                .find(|m| m.kind == table && m.id == id);
            if let Some(mapping) = mapping {
                row.insert("seq".to_owned(), Value::from(mapping.assigned_sequence));
            }
            // Saved summaries are retained verbatim as provenance, never made the
            // active summary of a different combined sequence range.
            if table != "decision_summaries" {
                if matches!(table, "plan_events" | "visual_feedback_events") {
                    row.remove("event_id");
                }
                insert_row(connection, table, &row)?;
            }
            connection.execute(
                "INSERT INTO planning_recovery_records(recovery_id,record_kind,record_id,original_sequence,assigned_sequence,original_json) VALUES(?1,?2,?3,?4,?5,?6)",
                params![recovery_id, table, id, original_sequence, mapping.map(|m| m.assigned_sequence), serde_json::to_string(original).map_err(|_| invalid("cannot encode recovery provenance"))?],
            )?;
        }
    }
    // Decisions' FTS triggers run with their original text; no fabricated task
    // completion, release delivery, or scheduler event is emitted by recovery.
    Ok(receipt)
}

#[cfg(test)]
mod tests;
