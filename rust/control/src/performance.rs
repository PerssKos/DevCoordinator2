use super::*;
use devcoordinator2_api::{
    outcomes::{OutcomeEffort, OutcomeMeasurement},
    performance as api,
};
use std::collections::BTreeSet;

impl ReviewService {
    pub(crate) fn performance_overview(
        &self,
        p: api::Overview,
        now: u64,
    ) -> Result<api::OverviewResult, ProtocolError> {
        window(p.window_start_ms, p.window_end_ms)?;
        if p.window_end_ms > now {
            return Err(invalid(
                "Performance window cannot include future measurements",
            ));
        }
        let repository = self.repository(&p.repository_id)?;
        let (total_tokens, coverage, usage) = if p.totals_only {
            if p.outcome_cursor.is_some() {
                return Err(invalid("Totals do not accept an outcome cursor"));
            }
            let report = self.usage.performance_tokens(
                &repository,
                p.window_start_ms,
                p.window_end_ms,
                now,
            )?;
            let value = report.totals.total_tokens;
            let measurement = OutcomeMeasurement {
                measured: value.unwrap_or(0),
                exact: value.filter(|_| !report.coverage.has_gaps),
                unknown: u64::from(value.is_none()),
            };
            (measurement, report.coverage, None)
        } else {
            let usage = self.performance_usage(
                &repository,
                &Prepare {
                    repository_id: p.repository_id.clone(),
                    workstream_id: None,
                    window_start_ms: p.window_start_ms,
                    window_end_ms: p.window_end_ms,
                    offset: 0,
                    limit: 1,
                    before_decision_seq: None,
                    outcome_cursor: p.outcome_cursor,
                    outcome_limit: p.outcome_limit,
                },
                Instant::now() + QUERY_TIMEOUT,
            )?;
            (
                usage.outcomes.totals.provider_total_tokens.clone(),
                usage.coverage.clone(),
                Some(usage),
            )
        };
        Ok(api::OverviewResult {
            repository_id: p.repository_id,
            window_start_ms: p.window_start_ms,
            window_end_ms: p.window_end_ms,
            generated_at_ms: now,
            total_tokens,
            coverage,
            usage,
        })
    }

    pub(crate) fn performance_reviews(
        &self,
        p: api::Reviews,
    ) -> Result<api::ReviewPage, ProtocolError> {
        page(0, p.limit)?;
        self.repository(&p.repository_id)?;
        if let Some(id) = &p.record_id {
            text(id, 1, 100)?;
        }
        let before = p.before.unwrap_or(i64::MAX as u64);
        if before > i64::MAX as u64 {
            return Err(invalid("Invalid review cursor"));
        }
        self.database.call(move |c| {
            let total_reviews: i64 = c.query_row("SELECT COUNT(DISTINCT record_id) FROM review_records WHERE repository_id=?1", [&p.repository_id], |r| r.get(0))?;
            let mut query = c.prepare("SELECT r.rowid,r.record_id,r.revision,r.recorded_at_ms,r.record_json,(SELECT COUNT(*) FROM review_records v WHERE v.repository_id=r.repository_id AND v.record_id=r.record_id) FROM review_records r WHERE r.repository_id=?1 AND r.rowid<?2 AND ((?3 IS NOT NULL AND r.record_id=?3) OR (?3 IS NULL AND NOT EXISTS(SELECT 1 FROM review_records v WHERE v.repository_id=r.repository_id AND v.record_id=r.record_id AND v.revision>r.revision))) ORDER BY r.rowid DESC LIMIT ?4")?;
            let mut rows = query.query(rusqlite::params![p.repository_id, before as i64, p.record_id, u32::from(p.limit)+1])?;
            let mut records = Vec::new(); let mut bytes = 0; let mut last = None; let mut more = false;
            while let Some(row) = rows.next()? {
                let encoded: String = row.get(4)?;
                if records.len() == usize::from(p.limit) || bytes + encoded.len() > 24_576 { more = true; break; }
                let record: ReviewRecord = serde_json::from_str(&encoded).map_err(|_| invalid("Stored review is invalid"))?;
                let record_id: String = row.get(1)?; let revision: u32 = row.get(2)?;
                let review = Revision { reference: format!("{record_id}@{revision}"), record_id, revision,
                    recorded_at_ms: row.get::<_, i64>(3)? as u64, completed: review_completed(&record), record };
                records.push(api::ReviewSummary { review, revision_count: row.get(5)? });
                bytes += encoded.len(); last = Some(row.get::<_, i64>(0)? as u64);
            }
            Ok(api::ReviewPage { records, total_reviews: total_reviews as u64, next_before: if more { last } else { None } })
        }).map_err(database_error)
    }

    pub(crate) fn performance_review(
        &self,
        p: api::Review,
        now: u64,
    ) -> Result<api::ReviewResult, ProtocolError> {
        let repository = self.repository(&p.repository_id)?;
        let review = self.receipt(Reference {
            reference: p.reference,
        })?;
        if review.record.repository_id != p.repository_id {
            return Err(invalid("Review belongs to another repository"));
        }
        let deadline = Instant::now() + QUERY_TIMEOUT;
        let record = &review.record;
        let params = Prepare {
            repository_id: p.repository_id.clone(),
            workstream_id: record.workstream_id.clone(),
            window_start_ms: record.window_start_ms,
            window_end_ms: record.window_end_ms,
            offset: 0,
            limit: 1,
            before_decision_seq: None,
            outcome_cursor: p.outcome_cursor.clone(),
            outcome_limit: p.outcome_limit,
        };
        let usage = self.performance_usage(&repository, &params, deadline)?;
        // Continuation pages only need the frozen outcome projection.
        let (evidence, comparisons) = if p.outcome_cursor.is_some() {
            (vec![], vec![])
        } else {
            (
                self.performance_evidence(record)?,
                self.performance_comparisons(&review, &usage, now, deadline)?,
            )
        };
        Ok(api::ReviewResult {
            review,
            usage,
            evidence,
            comparisons,
        })
    }

    fn performance_evidence(
        &self,
        record: &ReviewRecord,
    ) -> Result<Vec<api::Evidence>, ProtocolError> {
        let e = &record.experiment;
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        let outcome = record.outcome_id.as_ref().map(|id| EvidenceRef {
            kind: EvidenceKind::Outcome,
            reference: id.clone(),
        });
        for source in e
            .evidence_refs
            .iter()
            .chain(&e.baseline.evidence_refs)
            .chain(&e.result_evidence_refs)
            .chain(e.observations.iter().flat_map(|o| &o.evidence_refs))
            .chain(outcome.iter())
        {
            if !seen.insert(format!("{:?}:{}", source.kind, source.reference)) {
                continue;
            }
            let mut item = api::Evidence {
                source: source.clone(),
                title: format!("{:?} evidence", source.kind),
                body: String::new(),
                available: true,
            };
            if source.kind == EvidenceKind::Usage {
                item.title = "Canonical usage measurements".into();
            } else {
                item.available = self
                    .validate_reference(&record.repository_id, source.clone(), EvidenceUse::Context)
                    .is_ok();
                if item.available
                    && matches!(source.kind, EvidenceKind::Decision | EvidenceKind::Outcome)
                {
                    let repo = record.repository_id.clone();
                    let reference = source.reference.clone();
                    let kind = source.kind.clone();
                    let (title, body) = self.database.call(move |c| {
                        let query = if kind == EvidenceKind::Decision { "SELECT title,body FROM decisions WHERE repository_id=?1 AND (decision_id=?2 OR ref=?2)" }
                        else { "SELECT title,outcome FROM tasks WHERE repository_id=?1 AND task_id=?2" };
                        c.query_row(query, rusqlite::params![repo,reference], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(Into::into)
                    }).map_err(database_error)?;
                    item.title = title.chars().take(160).collect();
                    item.body = body.chars().take(1600).collect();
                }
            }
            result.push(item);
        }
        Ok(result)
    }

    fn performance_comparisons(
        &self,
        revision: &Revision,
        before: &ReviewUsage,
        now: u64,
        deadline: Instant,
    ) -> Result<Vec<api::Comparison>, ProtocolError> {
        let record = &revision.record;
        let e = &record.experiment;
        let repository = self.repository(&record.repository_id)?;
        let full_before;
        let before = if e
            .result_evidence_refs
            .iter()
            .any(|r| r.kind == EvidenceKind::Usage)
        {
            full_before = self.usage.review_window(
                &repository,
                record.workstream_id.as_deref(),
                record.window_start_ms,
                record.window_end_ms,
                deadline,
            )?;
            &full_before
        } else {
            before
        };
        let prepared = Prepared {
            repository_id: record.repository_id.clone(),
            workstream_id: record.workstream_id.clone(),
            window_start_ms: record.window_start_ms,
            window_end_ms: record.window_end_ms,
            generated_at_ms: now,
            source_refs: vec![usage_ref(
                &record.repository_id,
                record.window_start_ms,
                record.window_end_ms,
            )],
            coverage_gaps: vec![],
            usage: before.clone(),
            evidence: vec![],
            next_offset: None,
            standing_decisions: vec![],
            next_before_decision_seq: None,
            interpretation_rules: vec![],
        };
        // Apply the same quality, timestamp and workload checks as review recording.
        let validated = matches!(e.disposition, Disposition::Retained | Disposition::Reverted)
            && crate::review_validation::record(record).is_ok()
            && !e
                .baseline
                .missing_measurements
                .iter()
                .any(|gap| gap == "comparison_input_identity_unavailable")
            && self
                .validate_review_evidence(record, &prepared, revision.recorded_at_ms, deadline)
                .is_ok();
        let mut seen = BTreeSet::new();
        let mut comparisons = Vec::new();
        for source in &e.result_evidence_refs {
            if source.kind != EvidenceKind::Usage || !seen.insert(source.reference.clone()) {
                continue;
            }
            let Ok((start, end)) =
                evidence::result_window(&source.reference, record, revision.recorded_at_ms)
            else {
                continue;
            };
            let after = self.usage.review_window(
                &repository,
                record.workstream_id.as_deref(),
                start,
                end,
                deadline,
            )?;
            let metrics = metric_changes(&before.outcomes.totals, &after.outcomes.totals);
            let verified_improvement = validated
                && matches!(e.disposition, Disposition::Retained)
                && !before.coverage.has_gaps
                && !after.coverage.has_gaps
                && metrics.iter().any(|m| {
                    m.metric != "recorded_wait_ms" && m.reduction.is_some_and(|value| value > 0.0)
                });
            comparisons.push(api::Comparison { source: source.clone(), window_start_ms: start, window_end_ms: end, verified_improvement,
                explanation: if verified_improvement { "Comparable workload and passing quality evidence; measured changes are shown separately." }
                    else { "An improvement is not verified for this comparison. Check measurement coverage, workload evidence and the recorded disposition." }.into(), metrics });
        }
        Ok(comparisons)
    }
}

pub(super) fn metric_changes(
    before: &OutcomeEffort,
    after: &OutcomeEffort,
) -> Vec<api::MetricChange> {
    [
        (
            "tokens",
            &before.provider_total_tokens,
            &after.provider_total_tokens,
        ),
        (
            "active_agent_ms",
            &before.active_agent_ms,
            &after.active_agent_ms,
        ),
        (
            "elapsed_execution_ms",
            &before.elapsed_execution_ms,
            &after.elapsed_execution_ms,
        ),
        (
            "recorded_wait_ms",
            &before.recorded_wait_ms,
            &after.recorded_wait_ms,
        ),
    ]
    .into_iter()
    .map(|(metric, before, after)| {
        let reduction = before
            .exact
            .zip(after.exact)
            .map(|(a, b)| a as f64 - b as f64);
        let reduction_percent = reduction.and_then(|difference| {
            before
                .exact
                .filter(|value| *value > 0)
                .map(|baseline| difference / baseline as f64 * 100.0)
        });
        api::MetricChange {
            metric: metric.into(),
            before: before.clone(),
            after: after.clone(),
            reduction,
            reduction_percent,
        }
    })
    .collect()
}
