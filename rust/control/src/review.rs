use devcoordinator2_api::review::*;
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use std::time::Instant;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::database::Database;
use crate::review_validation::{database_error, invalid, page, text, window};
use crate::test_state::TestRunStore;
use crate::usage::{QUERY_TIMEOUT, RepositoryRecord, UsageService};

#[path = "review_evidence.rs"]
mod evidence;

#[derive(Clone)]
pub(crate) struct ReviewService {
    database: Database,
    usage: UsageService,
}

#[derive(Clone, Copy, PartialEq)]
enum EvidenceUse {
    Context,
    Baseline { end_ms: u64 },
    Result { start_ms: u64, end_ms: u64 },
}

impl ReviewService {
    pub(crate) fn new(database: Database, usage: UsageService) -> Self {
        Self { database, usage }
    }

    fn repository(&self, repository_id: &str) -> Result<RepositoryRecord, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database.call(move |connection| {
            Ok(connection.query_row(
                "SELECT repository_id,display_name,root_path FROM repositories WHERE repository_id=?1 AND archived_at IS NULL AND merged_into_repository_id IS NULL",
                [&repository_id], |row| Ok(RepositoryRecord {
                    repository_id: row.get(0)?, display_name: row.get(1)?,
                    root_path: std::path::PathBuf::from(row.get::<_, String>(2)?),
                }),
            ).optional()?)
        }).map_err(database_error)?.ok_or_else(|| ProtocolError::new(ErrorCode::RepositoryNotFound, "No active review repository"))
    }

    pub(crate) fn prepare(&self, params: Prepare, now_ms: u64) -> Result<Prepared, ProtocolError> {
        self.prepare_until(params, now_ms, Instant::now() + QUERY_TIMEOUT)
    }

    fn prepare_until(
        &self,
        params: Prepare,
        now_ms: u64,
        deadline: Instant,
    ) -> Result<Prepared, ProtocolError> {
        page(params.offset, params.limit)?;
        window(params.window_start_ms, params.window_end_ms)?;
        if params.window_end_ms > now_ms || now_ms > i64::MAX as u64 {
            return Err(invalid("Review window cannot include future measurements"));
        }
        if let Some(workstream) = &params.workstream_id {
            text(workstream, 1, 100)?;
        }
        let repository = self.repository(&params.repository_id)?;
        let usage = self.usage.review_window(
            &repository,
            params.window_start_ms,
            params.window_end_ms,
            deadline,
        )?;
        let mut gaps = vec![
            "task_usage_attribution_unavailable".to_owned(),
            "user_wait_measurement_unavailable".to_owned(),
        ];
        if params.workstream_id.is_some() {
            gaps.push("workstream_usage_attribution_unavailable".into());
        }
        if usage.coverage.has_gaps {
            gaps.push("canonical_usage_partial_or_unavailable".into());
        }
        if usage.totals.total_tokens.is_none() {
            gaps.push("total_tokens_unavailable".into());
        }
        if usage.time.summed_agent_active.unknown_intervals > 0 {
            gaps.push("active_time_partial".into());
        }
        let repository_id = params.repository_id.clone();
        let end = timestamp(params.window_end_ms)?;
        let (mut evidence, worktrees, decisions, next_decision) = self.database.call(move |connection| {
            let mut tasks = connection.prepare("SELECT task_id,title,status,outcome FROM tasks WHERE repository_id=?1 AND created_at<?2 ORDER BY seq LIMIT 1001")?;
            let evidence = tasks.query_map(rusqlite::params![repository_id,end], |row| Ok(Evidence {
                work: None,
                source: EvidenceRef { kind: EvidenceKind::Outcome, reference: row.get(0)? },
                source_sha256: None, config_sha256: None,
                title: row.get::<_, String>(1)?.chars().take(120).collect(), state: row.get(2)?,
                detail: row.get::<_, String>(3)?.chars().take(400).collect(),
            }))?.collect::<Result<Vec<_>, _>>()?;
            let mut paths = connection.prepare("SELECT worktree_id,worktree_path FROM worktrees WHERE repository_id=?1 ORDER BY worktree_id LIMIT 21")?;
            let worktrees = paths.query_map([&repository_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            let mut query = connection.prepare("SELECT decision_id,seq,title,body,superseded_by FROM decisions WHERE repository_id=?1 AND (?2 IS NULL OR seq<?2) ORDER BY seq DESC LIMIT ?3")?;
            let mut decisions = query.query_map(rusqlite::params![repository_id,params.before_decision_seq,u32::from(params.limit)+1], |row| Ok((row.get::<_, u32>(1)?, Evidence {
                work: None,
                source: EvidenceRef { kind: EvidenceKind::Decision, reference: row.get(0)? },
                source_sha256: None, config_sha256: None,
                title: row.get(2)?, state: row.get::<_, Option<String>>(4)?.map_or_else(|| "standing".into(), |next| format!("superseded_by:{next}")),
                detail: row.get::<_, String>(3)?.chars().take(400).collect(),
            })))?.collect::<Result<Vec<_>, _>>()?;
            let has_more = decisions.len() > usize::from(params.limit);
            decisions.truncate(usize::from(params.limit));
            let next = has_more.then(|| decisions.last().expect("nonempty page").0);
            Ok((evidence, worktrees, decisions.into_iter().map(|(_, evidence)| evidence).collect::<Vec<_>>(), next))
        }).map_err(database_error)?;
        if evidence.len() > 1000 {
            gaps.push("task_evidence_truncated".into());
            evidence.truncate(1000);
        }
        if worktrees.len() > 20 {
            gaps.push("worktree_evidence_truncated".into());
        }
        for (worktree_id, path) in worktrees.into_iter().take(20) {
            let retained = TestRunStore
                .read_evidence(std::path::Path::new(&path))
                .unwrap_or_default();
            let history = match TestRunStore.read_history(std::path::Path::new(&path)) {
                Ok(history) => history,
                Err(_) => {
                    gaps.push("retained_run_history_unavailable".into());
                    continue;
                }
            };
            for run in history {
                let Some(start) = unix_ms(&run.started_at) else {
                    gaps.push("run_timestamp_unavailable".into());
                    continue;
                };
                let finish = run
                    .finished_at
                    .as_deref()
                    .and_then(unix_ms)
                    .unwrap_or(now_ms);
                if start >= params.window_end_ms || finish < params.window_start_ms {
                    continue;
                }
                let identity = retained
                    .iter()
                    .find(|evidence| evidence.run_id == run.run_id);
                if identity.is_none() {
                    gaps.push("run_source_identity_unavailable".into());
                }
                if run
                    .work
                    .as_ref()
                    .and_then(|work| work.context.as_ref())
                    .is_none()
                {
                    gaps.push("run_work_context_unavailable".into());
                }
                evidence.push(Evidence {
                    work: run.work.clone(),
                    source: EvidenceRef {
                        kind: EvidenceKind::Run,
                        reference: format!("{worktree_id}/{}", run.run_id),
                    },
                    source_sha256: identity.map(|evidence| evidence.source_digest.clone()),
                    config_sha256: identity.map(|evidence| evidence.config_digest.clone()),
                    title: run.test,
                    state: serde_json::to_value(run.status)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    detail: format!(
                        "started_at_ms={start}; finished_at_ms={finish}; duration_seconds={:?}",
                        run.duration_seconds
                    ),
                });
            }
        }
        gaps.push("retained_history_may_expire".into());
        gaps.sort();
        gaps.dedup();
        evidence.sort_by(|left, right| left.source.reference.cmp(&right.source.reference));
        let start = usize::try_from(params.offset).unwrap_or(usize::MAX);
        let mut end = start
            .saturating_add(usize::from(params.limit))
            .min(evidence.len());
        if start > evidence.len() {
            return Err(invalid("Evidence offset is beyond the available page"));
        }
        let mut bytes = 0;
        for (offset, row) in evidence[start..end].iter().enumerate() {
            bytes += serde_json::to_vec(row)
                .map_err(|_| invalid("Cannot encode review evidence"))?
                .len();
            if bytes > 12_288 {
                if offset == 0 {
                    return Err(invalid(
                        "Review evidence entry exceeds the bounded packet size",
                    ));
                }
                end = start + offset;
                break;
            }
        }
        let next_offset = (end < evidence.len()).then_some(end as u32);
        Ok(Prepared {
            repository_id: params.repository_id.clone(), workstream_id: params.workstream_id,
            window_start_ms: params.window_start_ms, window_end_ms: params.window_end_ms, generated_at_ms: now_ms,
            source_refs: vec![usage_ref(&params.repository_id, params.window_start_ms, params.window_end_ms)],
            coverage_gaps: gaps,
            usage: ReviewUsage { coverage: usage.coverage, totals: usage.totals, activities: usage.activities,
                time: usage.time, tools: usage.tools, semantics: usage.semantics },
            evidence: evidence[start..end].to_vec(), next_offset,
            standing_decisions: decisions, next_before_decision_seq: next_decision,
            interpretation_rules: vec![
                "User waiting is not avoidable agent work.".into(),
                "Intentional repeated release validation is not waste; preserve required quality gates.".into(),
                "Repository totals do not establish per-task attribution or explain causality.".into(),
                "Decision excerpts are bounded; follow decision.tail/search for full rationale and supersession.".into(),
            ],
        })
    }

    pub(crate) fn record(
        &self,
        params: Record,
        actor: &str,
        now_ms: u64,
    ) -> Result<Revision, ProtocolError> {
        crate::review_validation::record(&params.record)?;
        let encoded =
            serde_json::to_string(&params.record).map_err(|_| invalid("Cannot encode review"))?;
        if encoded.len() > 8192 {
            return Err(invalid("Review record must fit within 8 KiB"));
        }
        let record = &params.record;
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let prepared = self.prepare_until(
            Prepare {
                repository_id: record.repository_id.clone(),
                workstream_id: record.workstream_id.clone(),
                window_start_ms: record.window_start_ms,
                window_end_ms: record.window_end_ms,
                offset: 0,
                limit: 1,
                before_decision_seq: None,
            },
            now_ms,
            deadline,
        )?;
        if prepared.coverage_gaps.iter().any(|gap| {
            !record
                .experiment
                .baseline
                .missing_measurements
                .contains(gap)
        }) {
            return Err(invalid(
                "Baseline must acknowledge each current prepare coverage gap",
            ));
        }
        self.validate_review_evidence(record, &prepared, now_ms, deadline)?;
        let record_id = match params.record_id {
            Some(record_id) => {
                text(&record_id, 1, 100)?;
                record_id
            }
            None => format!(
                "review-{}",
                crate::ids::decision_id()
                    .map_err(|_| invalid("Cannot allocate review identity"))?
            ),
        };
        let revision = params
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| invalid("Revision overflow"))?;
        let completed = review_completed(record);
        let result = Revision {
            reference: format!("{record_id}@{revision}"),
            record_id: record_id.clone(),
            revision,
            recorded_at_ms: now_ms,
            completed,
            record: params.record,
        };
        let stored = result.clone();
        let actor = actor.to_owned();
        self.database.transaction(move |transaction| {
            let previous = transaction.query_row("SELECT repository_id,revision,record_json FROM review_records WHERE record_id=?1 ORDER BY revision DESC LIMIT 1", [&record_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, String>(2)?))).optional()?;
            if previous.as_ref().map_or(0, |(_, revision, _)| *revision) != params.expected_revision {
                return Err(invalid("Review revision conflict; read the current revision before appending").into());
            }
            if let Some((repository_id, _, previous)) = previous {
                let previous: ReviewRecord = serde_json::from_str(&previous).map_err(|_| invalid("Stored review is invalid"))?;
                if repository_id != stored.record.repository_id || previous.workstream_id != stored.record.workstream_id
                    || previous.outcome_id != stored.record.outcome_id || previous.window_start_ms != stored.record.window_start_ms || previous.window_end_ms != stored.record.window_end_ms {
                    return Err(invalid("Review revisions cannot change repository, workstream or window identity").into());
                }
            }
            transaction.execute("INSERT INTO review_records(record_id,revision,repository_id,window_start_ms,window_end_ms,record_json,actor,recorded_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                rusqlite::params![record_id,revision,stored.record.repository_id,stored.record.window_start_ms as i64,stored.record.window_end_ms as i64,encoded,actor,now_ms as i64])?;
            Ok(())
        }).map_err(database_error)?;
        Ok(result)
    }

    pub(crate) fn show(&self, params: Show) -> Result<Page, ProtocolError> {
        page(params.offset, params.limit)?;
        self.repository(&params.repository_id)?;
        self.database.call(move |connection| {
            let mut query = connection.prepare("SELECT record_id,revision,recorded_at_ms,record_json FROM review_records WHERE repository_id=?1 AND (?2 IS NULL OR record_id=?2) ORDER BY rowid LIMIT ?3 OFFSET ?4")?;
            let mut rows = query.query(rusqlite::params![params.repository_id,params.record_id,u32::from(params.limit)+1,params.offset])?;
            let mut records = Vec::new();
            let mut bytes = 0;
            let mut more = false;
            while let Some(row) = rows.next()? {
                let encoded: String = row.get(3)?;
                if records.len() == usize::from(params.limit) || bytes + encoded.len() > 24_576 { more = true; break; }
                bytes += encoded.len();
                let record: ReviewRecord = serde_json::from_str(&encoded).map_err(|_| invalid("Stored review is invalid"))?;
let completed = review_completed(&record);
                let record_id: String = row.get(0)?;
                let revision: u32 = row.get(1)?;
                records.push(Revision { reference: format!("{record_id}@{revision}"), record_id, revision, recorded_at_ms: row.get::<_, i64>(2)? as u64, completed, record });
            }
            Ok(Page { next_offset: more.then_some(params.offset + records.len() as u32), records })
        }).map_err(database_error)
    }

    pub(crate) fn receipt(&self, params: Reference) -> Result<Revision, ProtocolError> {
        text(&params.reference, 3, 120)?;
        let (record_id, revision) = params
            .reference
            .rsplit_once('@')
            .ok_or_else(|| invalid("Review reference must be record_id@revision"))?;
        let revision: u32 = revision
            .parse()
            .map_err(|_| invalid("Invalid review revision"))?;
        let record_id = record_id.to_owned();
        self.database.call(move |connection| {
            let (encoded, recorded_at_ms) = connection.query_row("SELECT record_json,recorded_at_ms FROM review_records WHERE record_id=?1 AND revision=?2", rusqlite::params![record_id,revision],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))).optional()?.ok_or_else(|| invalid("Review revision not found"))?;
            let record: ReviewRecord = serde_json::from_str(&encoded).map_err(|_| invalid("Stored review is invalid"))?;
let completed = review_completed(&record);
            Ok(Revision { reference: params.reference, record_id, revision, recorded_at_ms, completed, record })
        }).map_err(database_error)
    }
}

fn review_completed(record: &ReviewRecord) -> bool {
    match record.experiment.disposition {
        Disposition::Retained | Disposition::Reverted => {
            record.experiment.comparison.is_some()
                && crate::review_validation::record(record).is_ok()
        }
        Disposition::Inconclusive | Disposition::Unchanged => true,
        Disposition::Proposed | Disposition::Applied => false,
    }
}

pub(crate) fn usage_ref(repository: &str, start: u64, end: u64) -> EvidenceRef {
    EvidenceRef {
        kind: EvidenceKind::Usage,
        reference: format!("canonical-usage:{repository}:{start}:{end}"),
    }
}

pub(crate) fn timestamp(milliseconds: u64) -> Result<String, ProtocolError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
        .map_err(|_| invalid("Timestamp is out of range"))?
        .format(&Rfc3339)
        .map_err(|_| invalid("Timestamp is out of range"))
}

fn unix_ms(value: &str) -> Option<u64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .and_then(|time| u64::try_from(time.unix_timestamp_nanos() / 1_000_000).ok())
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "review_result_tests.rs"]
mod result_tests;
