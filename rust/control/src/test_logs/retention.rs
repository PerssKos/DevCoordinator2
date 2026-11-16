//! Resumable automatic retention. The filesystem walker and every SQL page
//! have explicit bounds; one generation keeps depth ranks fixed until selection
//! finishes. Cursor state belongs to the existing maintenance job.
use super::{
    MaintenanceReport, MaintenanceStep, TestLogService, active_run_id, database_error,
    push_maintenance_error, timestamp,
};
use crate::database::{OptionalExtension, params};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use devcoordinator2_executor_core::log_query::{
    LogPruneRequest, LogQueryError, RetentionObservation, RetentionPruneOutcome,
    collect_retention_garbage_step, discover_retention_step, prune_retention_observation,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DISCOVERY_ENTRIES: usize = 16;
const PAGE_ROWS: i64 = 64;
const PAGE_BYTES: usize = 256 * 1024;
const OBSERVATION_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Discover,
    Select,
    Cleanup,
    Garbage,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    after_worktree: String,
    worktree: Option<String>,
    generation: String,
    phase: Phase,
    filesystem: Option<String>,
    after_path: String,
    max_age_seconds: u32,
    case_depth: u16,
    skip_filesystem: bool,
    restart_changed: bool,
    #[serde(default)]
    error: Option<String>,
}
impl Default for Cursor {
    fn default() -> Self {
        Self {
            version: 1,
            after_worktree: String::new(),
            worktree: None,
            generation: String::new(),
            phase: Phase::Discover,
            filesystem: None,
            after_path: String::new(),
            max_age_seconds: 0,
            case_depth: 0,
            skip_filesystem: false,
            restart_changed: false,
            error: None,
        }
    }
}
impl Cursor {
    fn start(
        &mut self,
        service: &TestLogService,
        id: String,
        age: u32,
        depth: u16,
    ) -> Result<(), ProtocolError> {
        let mut nonce = [0_u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| unavailable())?;
        self.worktree = Some(id.clone());
        self.generation = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        self.phase = Phase::Discover;
        self.filesystem = None;
        self.after_path.clear();
        self.max_age_seconds = age;
        self.case_depth = depth;
        self.skip_filesystem = false;
        self.restart_changed = false;
        service.database.maintenance_transaction(|tx|{
            tx.execute("INSERT INTO dc2_test_log_retention_heads(worktree_id,generation,status,max_age_seconds,case_depth,published_at_ms) VALUES($1,$2,'building',$3,$4,NULL) ON CONFLICT(worktree_id) DO UPDATE SET generation=excluded.generation,status='building',max_age_seconds=excluded.max_age_seconds,case_depth=excluded.case_depth,published_at_ms=NULL",params![id,self.generation,age,depth])?;Ok(())
        }).map_err(database_error)?;
        Ok(())
    }
    fn advance(&mut self) {
        if let Some(id) = self.worktree.take() {
            self.after_worktree = id;
        }
        self.generation.clear();
        self.filesystem = None;
        self.after_path.clear();
        self.phase = Phase::Discover;
        self.skip_filesystem = false;
        self.restart_changed = false;
    }
}
fn unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestLogUnavailable,
        "Governed test log maintenance is unavailable.",
    )
}
fn report() -> MaintenanceReport {
    MaintenanceReport {
        removed_leaf_folders: 0,
        retained_active: 0,
        next_expiry_at: None,
        skipped_missing_worktrees: 0,
        errors: Vec::new(),
        errors_truncated: false,
    }
}
fn remember_expiry(report: &mut MaintenanceReport, at: u64) {
    if let Ok(time) = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        && let Ok(value) = time.format(&time::format_description::well_known::Rfc3339)
        && report
            .next_expiry_at
            .as_ref()
            .is_none_or(|old| &value < old)
    {
        report.next_expiry_at = Some(value);
    }
}

pub(super) fn step(
    service: &TestLogService,
    input: Option<String>,
    _max_worktrees: u16,
    budget: Duration,
) -> Result<MaintenanceStep, ProtocolError> {
    let started = Instant::now();
    let first = input.is_none();
    let mut cursor = match input.as_deref() {
        None => Cursor::default(),
        Some(value) if value.starts_with('{') && value.len() <= PAGE_BYTES => {
            serde_json::from_str::<Cursor>(value).map_err(|_| unavailable())?
        }
        Some(value) if !value.starts_with('{') && value.len() <= 64 => Cursor {
            after_worktree: value.to_owned(),
            ..Cursor::default()
        },
        _ => return Err(unavailable()),
    };
    if cursor.version != 1 {
        return Err(unavailable());
    }
    let settings = service.retention()?;
    let mut result = report();
    let mut visited = 0_u64;
    let mut bytes = 0_u64;
    let owner=service.database.read(|c|{
  let sql=if cursor.worktree.is_some(){"SELECT worktree_path,repository_id,worktree_id FROM worktrees WHERE worktree_id=$1"}else{"SELECT worktree_path,repository_id,worktree_id FROM worktrees WHERE worktree_id>$1 ORDER BY worktree_id LIMIT 1"};
  Ok(c.query_row(sql,[cursor.worktree.as_deref().unwrap_or(&cursor.after_worktree)],|r|Ok((PathBuf::from(r.get::<_,String>(0)?),r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?)
 }).map_err(database_error)?;
    let Some((worktree, repository, id)) = owner else {
        if cursor.worktree.is_some() {
            cursor.advance();
            return finish(service, cursor, result, first, false, visited, bytes);
        }
        return finish(service, cursor, result, first, true, visited, bytes);
    };
    let generation_exists=cursor.worktree.is_some()&&service.database.read(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM dc2_test_log_retention_heads WHERE worktree_id=$1 AND generation=$2)",params![id,cursor.generation],|r|r.get::<_,bool>(0))?)).map_err(database_error)?;
    if !generation_exists
        || cursor.max_age_seconds != settings.max_age_seconds
        || cursor.case_depth != settings.case_depth
    {
        cursor.start(
            service,
            id.clone(),
            settings.max_age_seconds,
            settings.case_depth,
        )?;
    }
    if !cursor.skip_filesystem {
        match std::fs::symlink_metadata(&worktree) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                result.skipped_missing_worktrees = 1;
                cursor.skip_filesystem = true;
                cursor.phase = Phase::Cleanup;
            }
            _ => {
                push_maintenance_error(&mut result.errors, &mut 0, repository.clone());
                cursor.skip_filesystem = true;
                cursor.phase = Phase::Cleanup;
            }
        }
    }
    let request = if cursor.skip_filesystem {
        None
    } else {
        match active_run_id(&worktree) {
            Ok(active_run_id) => Some(LogPruneRequest {
                schema: 2,
                repository_id: repository.clone(),
                max_age_seconds: u64::from(settings.max_age_seconds),
                case_depth: u64::from(settings.case_depth),
                active_run_id,
            }),
            Err(_) => {
                push_maintenance_error(&mut result.errors, &mut 0, repository.clone());
                cursor.skip_filesystem = true;
                cursor.phase = Phase::Cleanup;
                None
            }
        }
    };
    match cursor.phase {
        Phase::Discover => {
            let discovered = discover_retention_step(
                &worktree,
                request.as_ref().ok_or_else(unavailable)?,
                cursor.filesystem.as_deref(),
                DISCOVERY_ENTRIES,
                budget.saturating_sub(started.elapsed()),
            );
            match discovered {
                Ok(discovered) => {
                    visited = discovered.visited_entries as u64;
                    let mut encoded = Vec::with_capacity(discovered.observations.len());
                    for observation in discovered.observations {
                        let json =
                            serde_json::to_string(&observation).map_err(|_| unavailable())?;
                        if json.len() > OBSERVATION_BYTES
                            || bytes.saturating_add(json.len() as u64) > PAGE_BYTES as u64
                        {
                            return Err(unavailable());
                        }
                        bytes += json.len() as u64;
                        encoded.push((observation, json));
                    }
                    service.database.maintenance_transaction(|tx|{
      for (observation,json) in encoded{tx.execute("INSERT INTO test_log_retention_projection(worktree_id,generation,relative_path,run_id,history_key,finished_at_ms,active,location_json,removed,eligible) VALUES($1,$2,$3,$4,$5,$6,$7,$8,0,NULL) ON CONFLICT(worktree_id,relative_path) DO UPDATE SET generation=excluded.generation,run_id=excluded.run_id,history_key=excluded.history_key,finished_at_ms=excluded.finished_at_ms,active=excluded.active,location_json=excluded.location_json,removed=0,eligible=NULL",params![id,cursor.generation,observation.components.join("/"),observation.run_id,observation.history_key,observation.finished_at_ms,observation.active,json])?;}
      Ok(())
     }).map_err(database_error)?;
                    cursor.filesystem = discovered.next_cursor;
                    if cursor.filesystem.is_none() {
                        cursor.phase = Phase::Select;
                        cursor.after_path.clear();
                    }
                }
                Err(LogQueryError::MaintenanceBusy) => return Err(unavailable()),
                Err(LogQueryError::CursorStale) => {
                    push_maintenance_error(&mut result.errors, &mut 0, repository.clone());
                    cursor.start(
                        service,
                        id.clone(),
                        settings.max_age_seconds,
                        settings.case_depth,
                    )?;
                }
                Err(_) => {
                    push_maintenance_error(&mut result.errors, &mut 0, repository.clone());
                    cursor.phase = Phase::Cleanup;
                    cursor.skip_filesystem = true;
                }
            }
        }
        Phase::Select => {
            let limit = (4096_i64 / i64::from(settings.case_depth.max(1))).clamp(1, PAGE_ROWS);
            let selected=service.database.read(|c|{
    let mut statement=c.prepare("WITH page AS MATERIALIZED(SELECT relative_path,location_json,active,finished_at_ms,removed,history_key,run_id,octet_length(location_json::text) AS bytes FROM test_log_retention_projection WHERE worktree_id=$1 AND generation=$2 AND relative_path>$3 ORDER BY relative_path LIMIT $5), budget AS MATERIALIZED(SELECT *,row_number() OVER(ORDER BY relative_path) ordinal,sum(bytes) OVER(ORDER BY relative_path ROWS UNBOUNDED PRECEDING) used FROM page) SELECT p.relative_path,p.location_json,p.active,p.finished_at_ms,p.removed,CASE WHEN p.active=0 THEN EXISTS(SELECT 1 FROM test_log_retention_projection n WHERE n.worktree_id=$1 AND n.generation=$2 AND n.history_key=p.history_key AND n.active=0 AND (n.finished_at_ms,n.run_id,n.relative_path)>(p.finished_at_ms,p.run_id,p.relative_path) ORDER BY n.finished_at_ms DESC,n.run_id DESC,n.relative_path DESC LIMIT 1 OFFSET ($4::bigint-1)) ELSE false END FROM budget p WHERE used<=262144 OR ordinal=1 ORDER BY p.relative_path")?;
    Ok(statement.query_map(params![id,cursor.generation,cursor.after_path,settings.case_depth,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,u64>(3)?,r.get::<_,bool>(4)?,r.get::<_,bool>(5)?)))?.collect::<crate::database::Result<Vec<_>>>()?)
   }).map_err(database_error)?;
            let now = service
                .clock
                .now_utc()
                .unix_timestamp_nanos()
                .div_euclid(1_000_000)
                .max(0) as u64;
            for (path, json, active, finished, removed, beyond) in selected {
                if visited > 0 && started.elapsed() >= budget {
                    break;
                }
                visited += 1;
                bytes = bytes.saturating_add(json.len() as u64);
                let expiry = finished.saturating_add(u64::from(settings.max_age_seconds) * 1000);
                let eligible = !active && (beyond || expiry <= now);
                service.database.maintenance_transaction(|tx|{tx.execute("UPDATE test_log_retention_projection SET eligible=$1 WHERE worktree_id=$2 AND generation=$3 AND relative_path=$4",params![eligible,id,cursor.generation,path])?;Ok(())}).map_err(database_error)?;
                if active {
                    result.retained_active += 1;
                    cursor.after_path = path;
                    continue;
                }
                if !removed && !beyond && expiry > now {
                    remember_expiry(&mut result, expiry);
                    cursor.after_path = path;
                    continue;
                }
                let observation: RetentionObservation =
                    serde_json::from_str(&json).map_err(|_| unavailable())?;
                let outcome = if removed {
                    RetentionPruneOutcome::Missing
                } else {
                    prune_retention_observation(
                        &worktree,
                        request.as_ref().ok_or_else(unavailable)?,
                        &observation,
                        || {
                            crate::storage::evidence::run_is_protected(
                                &service.database,
                                &worktree,
                                &observation.run_id,
                            )
                            .map_err(|_| LogQueryError::Unavailable)
                        },
                    )
                    .map_err(super::log_error)?
                };
                match outcome {
                    RetentionPruneOutcome::Removed | RetentionPruneOutcome::Missing => {
                        service.database.maintenance_transaction(|tx|{tx.execute("UPDATE test_log_retention_projection SET removed=1 WHERE worktree_id=$1 AND generation=$2 AND relative_path=$3",params![id,cursor.generation,path])?;Ok(())}).map_err(database_error)?;
                        let all_removed=service.database.read(|c|Ok(c.query_row("SELECT NOT EXISTS(SELECT 1 FROM test_log_retention_projection WHERE worktree_id=$1 AND generation=$2 AND run_id=$3 AND removed=0)",params![id,cursor.generation,observation.run_id],|r|r.get::<_,bool>(0))?)).map_err(database_error)?;
                        if all_removed {
                            crate::storage::evidence::record_run_expired(
                                &service.database,
                                &worktree,
                                &observation.run_id,
                            )?;
                        }
                        if outcome == RetentionPruneOutcome::Removed {
                            result.removed_leaf_folders += 1;
                        }
                    }
                    RetentionPruneOutcome::Active => {
                        result.retained_active += 1;
                        service.database.maintenance_transaction(|tx|{tx.execute("UPDATE test_log_retention_projection SET active=1,eligible=0 WHERE worktree_id=$1 AND generation=$2 AND relative_path=$3",params![id,cursor.generation,path])?;Ok(())}).map_err(database_error)?;
                    }
                    RetentionPruneOutcome::Protected => {}
                    RetentionPruneOutcome::Changed => {
                        cursor.restart_changed = true;
                    }
                }
                cursor.after_path = path;
            }
            let remains=service.database.read(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM test_log_retention_projection WHERE worktree_id=$1 AND generation=$2 AND relative_path>$3)",params![id,cursor.generation,cursor.after_path],|r|r.get::<_,bool>(0))?)).map_err(database_error)?;
            if !remains {
                if !cursor.restart_changed {
                    service.database.maintenance_transaction(|tx|{tx.execute("UPDATE dc2_test_log_retention_heads SET status='published',published_at_ms=$1 WHERE worktree_id=$2 AND generation=$3",params![now,id,cursor.generation])?;Ok(())}).map_err(database_error)?;
                }
                cursor.phase = Phase::Cleanup;
                cursor.filesystem = None;
            }
        }
        Phase::Cleanup => {
            let count=service.database.maintenance_transaction(|tx|{
                if cursor.skip_filesystem{
                    Ok(tx.execute("WITH victims AS MATERIALIZED(SELECT ctid FROM test_log_retention_projection WHERE worktree_id=$1 ORDER BY relative_path LIMIT 64) DELETE FROM test_log_retention_projection p USING victims v WHERE p.ctid=v.ctid",[&id])?)
                }else{
                    Ok(tx.execute("WITH older AS MATERIALIZED(SELECT ctid,generation,relative_path FROM test_log_retention_projection WHERE worktree_id=$1 AND generation<$2 ORDER BY generation,relative_path LIMIT 64),newer AS MATERIALIZED(SELECT ctid,generation,relative_path FROM test_log_retention_projection WHERE worktree_id=$1 AND generation>$2 ORDER BY generation,relative_path LIMIT 64),removed AS MATERIALIZED(SELECT ctid,generation,relative_path FROM test_log_retention_projection WHERE worktree_id=$1 AND generation=$2 AND removed=1 ORDER BY relative_path LIMIT 64),victims AS MATERIALIZED(SELECT ctid FROM(SELECT * FROM older UNION ALL SELECT * FROM newer UNION ALL SELECT * FROM removed) bounded LIMIT 64) DELETE FROM test_log_retention_projection p USING victims v WHERE p.ctid=v.ctid",params![id,cursor.generation])?)
                }
            }).map_err(database_error)?;
            let remains = count == 64;
            visited = count as u64;
            if !remains {
                if cursor.skip_filesystem {
                    cursor.advance();
                } else {
                    cursor.phase = Phase::Garbage;
                    cursor.filesystem = None;
                }
            }
        }
        Phase::Garbage => {
            match collect_retention_garbage_step(
                &worktree,
                cursor.filesystem.as_deref(),
                16,
                budget.saturating_sub(started.elapsed()),
            ) {
                Ok(garbage) => {
                    visited = garbage.visited_entries as u64;
                    cursor.filesystem = garbage.next_cursor;
                    if cursor.filesystem.is_none() {
                        if let Err(error) = devcoordinator2_executor_core::log_query::prune_empty_retention_containers(
                            &worktree,
                        ) && !matches!(error, LogQueryError::LogNotFound) {
                            push_maintenance_error(
                                &mut result.errors,
                                &mut 0,
                                repository.clone(),
                            );
                        }
                        if cursor.restart_changed {
                            cursor.start(
                                service,
                                id,
                                settings.max_age_seconds,
                                settings.case_depth,
                            )?;
                        } else {
                            cursor.advance();
                        }
                    }
                }
                Err(LogQueryError::MaintenanceBusy) => return Err(unavailable()),
                Err(LogQueryError::CursorStale) => cursor.filesystem = None,
                Err(_) => {
                    push_maintenance_error(&mut result.errors, &mut 0, repository);
                    cursor.advance();
                }
            }
        }
    }
    finish(service, cursor, result, first, false, visited, bytes)
}
fn finish(
    service: &TestLogService,
    mut cursor: Cursor,
    report: MaintenanceReport,
    _first: bool,
    complete: bool,
    visited: u64,
    bytes: u64,
) -> Result<MaintenanceStep, ProtocolError> {
    let at = timestamp(service.clock.as_ref())?;
    if cursor.error.is_none() {
        cursor.error = report.errors.first().map(|e| e.code.clone());
    }
    service.database.maintenance_transaction(|tx|{tx.execute("UPDATE test_log_retention_state SET last_cleanup_at=CASE WHEN $3::boolean THEN $1 ELSE last_cleanup_at END,last_cleanup_error_code=CASE WHEN $3::boolean THEN $2 ELSE COALESCE($2,last_cleanup_error_code) END WHERE singleton=1",params![at,cursor.error,complete])?;Ok(())}).map_err(database_error)?;
    let next = if complete {
        None
    } else {
        let value = serde_json::to_string(&cursor).map_err(|_| unavailable())?;
        if value.len() > PAGE_BYTES {
            return Err(unavailable());
        }
        Some(value)
    };
    Ok(MaintenanceStep {
        report,
        next_worktree_id: next,
        complete,
        visited_entries: visited,
        processed_bytes: bytes,
    })
}
