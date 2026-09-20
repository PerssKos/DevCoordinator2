use super::review_facts::Facts;
use super::review_math::Effort;
use super::*;
use devcoordinator2_api::outcomes::{OutcomeReport, OutcomeRow};

#[derive(Default)]
pub(super) struct Groups {
    total: Effort,
    attributed: Effort,
    unattributed: Effort,
    rows: BTreeMap<(String, Option<String>), Effort>,
    reasons: BTreeMap<String, u64>,
}

impl Groups {
    pub(super) fn merge(&mut self, other: Self, source: usize) -> Result<(), String> {
        self.total.merge(other.total, source)?;
        self.attributed.merge(other.attributed, source)?;
        self.unattributed.merge(other.unattributed, source)?;
        for (key, effort) in other.rows {
            self.rows.entry(key).or_default().merge(effort, source)?;
        }
        merge_counts(&mut self.reasons, &other.reasons);
        Ok(())
    }

    pub(super) fn legacy(report: &SourceReport) -> Self {
        let effort = Effort {
            operations: report.operation_count,
            tokens: report.tokens.get("total_tokens").copied().unwrap_or(0),
            unknown_tokens: report
                .token_observations
                .iter()
                .filter(|(key, _)| key.as_str() != "complete")
                .map(|(_, count)| *count)
                .sum(),
            elapsed: report.execution_intervals.clone(),
            active: report.agent_intervals.clone(),
            unknown_elapsed: report.execution_unknown,
            unknown_active: report.agent_unknown,
            // Older readers have no independent wait union. Do not claim zero.
            unknown_waits: 1,
            ..Default::default()
        };
        Self {
            total: effort.clone(),
            unattributed: effort,
            reasons: BTreeMap::from([("legacy_collector".into(), report.operation_count)]),
            ..Default::default()
        }
    }

    pub(super) fn finish(self, source_gaps: bool) -> Result<OutcomeReport, String> {
        let mut totals = self.total.finish()?;
        let mut attributed = self.attributed.finish()?;
        let mut unattributed = self.unattributed.finish()?;
        let coverage = if attributed.operations == 0 {
            "unavailable"
        } else if source_gaps
            || unattributed.operations > 0
            || totals.provider_total_tokens.unknown > 0
            || totals.active_agent_ms.unknown > 0
            || totals.elapsed_execution_ms.unknown > 0
            || totals.recorded_wait_ms.unknown > 0
        {
            "partial"
        } else {
            "complete"
        };
        if source_gaps {
            for effort in [&mut totals, &mut attributed, &mut unattributed] {
                mark_incomplete(effort);
            }
        }
        let rows = self
            .rows
            .into_iter()
            .map(|((outcome_id, workstream_id), effort)| {
                Ok(OutcomeRow {
                    title: None,
                    outcome_id,
                    workstream_id,
                    effort: {
                        let mut effort = effort.finish()?;
                        if source_gaps {
                            mark_incomplete(&mut effort);
                        }
                        effort
                    },
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(OutcomeReport {
            schema_version: 1, coverage: coverage.into(), totals, attributed, unattributed,
            unattributed_reasons: self.reasons, total_rows: rows.len(), rows, next_cursor: None,
            basis: "Prospective operation-start declarations only. Provider totals deduplicate by owner and source event; component categories are not added. Time is clipped to the UTC window. Active time sums per-agent unions after recorded waits; elapsed and waiting time are unions, not sums of outcome rows. Unknown capture and unrecorded waits cannot be reconstructed. Repository aggregates retain their repository scope; outcome rows honor the requested workstream, with undeclared work explicit.".into(),
        })
    }
}

fn mark_incomplete(effort: &mut devcoordinator2_api::outcomes::OutcomeEffort) {
    for metric in [
        &mut effort.provider_total_tokens,
        &mut effort.active_agent_ms,
        &mut effort.elapsed_execution_ms,
        &mut effort.recorded_wait_ms,
    ] {
        metric.exact = None;
    }
}

pub(super) fn aggregate(
    connection: &Connection,
    mut facts: Facts,
    workstream: Option<&str>,
    start_ms: u64,
    end_ms: u64,
) -> Result<(SourceReport, Groups), String> {
    let mut report = SourceReport {
        database_schema: 7,
        taxonomy_version: 1,
        phase_series: vec![BTreeMap::new()],
        token_buckets_observed: vec![false],
        bucket_coverage: vec![CoverageState::Unobserved],
        ..Default::default()
    };
    let mut counts = HashMap::<String, (u64, u64)>::new();
    for token in facts.tokens {
        let Some(owner) = facts.operations.get(&token.owner) else {
            continue;
        };
        if !TOKEN_CATEGORIES
            .iter()
            .any(|(category, _)| *category == token.category)
        {
            continue;
        }
        report.evidence = true;
        increment(
            &mut report.token_observations,
            if token.incomplete {
                "partial"
            } else {
                "complete"
            },
        );
        if let Some(value) = token.value {
            let count = report.tokens.entry(token.category.clone()).or_default();
            *count = count.checked_add(value).ok_or("usage_overflow")?;
        }
        if token.category != "total_tokens" {
            continue;
        }
        let count = counts.entry(token.owner).or_default();
        count.0 = count
            .0
            .checked_add(token.value.unwrap_or(0))
            .ok_or("usage_overflow")?;
        count.1 += u64::from(token.incomplete);
        report.token_buckets_observed[0] = true;
        report.bucket_coverage[0] =
            if token.incomplete || report.bucket_coverage[0] == CoverageState::Partial {
                CoverageState::Partial
            } else {
                CoverageState::Complete
            };
        if let Some(value) = token.value {
            *report.phase_series[0]
                .entry(owner.operation.phase.clone())
                .or_default() += value;
            increment_pair(
                &mut report.activities,
                (&owner.operation.phase, &owner.operation.activity),
                value,
            );
        }
    }
    let mut groups = Groups::default();
    let mut all = Effort::default();
    let mut by_id = HashMap::new();
    for (id, operation) in facts.operations {
        let (tokens, unknown) = counts
            .get(&id)
            .copied()
            .unwrap_or((0, u64::from(operation.operation.kind == "model_request")));
        let (waits, unknown_waits) = facts.waits.remove(&id).unwrap_or_default();
        all.add_time(&operation, &waits, unknown_waits)?;
        if operation.operation.kind == "model_request" && !counts.contains_key(&id) {
            increment(&mut report.token_observations, "unknown");
        }
        if operation.overlaps_window {
            report.evidence = true;
            report.operation_count += 1;
            report.freshest_at_ms = Some(
                report
                    .freshest_at_ms
                    .unwrap_or(0)
                    .max(operation.operation.started_at_ms),
            );
            let detail = &operation.operation;
            if detail.kind == "model_request" {
                report.model_request_count += 1;
                if let Some(interval) = operation.interval {
                    report.request_intervals.push(interval);
                } else {
                    report.request_unknown += 1;
                }
            } else if matches!(
                detail.kind.as_str(),
                "local_tool" | "hosted_tool" | "activity_control"
            ) {
                report.tool_count += 1;
                increment(
                    &mut report.tool_outcomes,
                    tool_outcome(detail.terminal_event.as_deref()),
                );
                increment(
                    &mut report.tool_families,
                    detail.tool_family.as_deref().unwrap_or("unknown"),
                );
            }
            if let Some(interval) = operation.interval {
                report
                    .phase_intervals
                    .entry(detail.phase.clone())
                    .or_default()
                    .push(interval);
            } else {
                increment(&mut report.phase_unknown, &detail.phase);
            }
            increment_pair(
                &mut report.activity_operations,
                (&detail.phase, &detail.activity),
                1,
            );
            increment_triple(
                &mut report.activity_provenance,
                (&detail.phase, &detail.activity, &detail.provenance),
                1,
            );
        }
        let include = workstream.is_none_or(|selected| {
            operation
                .workstream
                .as_deref()
                .is_none_or(|value| value == selected)
        });
        if include {
            let mut targets = vec![&mut groups.total];
            if let Some(key) = &operation.key
                && (workstream.is_none() || operation.workstream.is_some())
            {
                targets.push(&mut groups.attributed);
                targets.push(groups.rows.entry(key.clone()).or_default());
            } else {
                targets.push(&mut groups.unattributed);
                increment(
                    &mut groups.reasons,
                    operation.reason.unwrap_or("workstream_not_declared"),
                );
            }
            for target in targets {
                target.add_time(&operation, &waits, unknown_waits)?;
                target.tokens = target.tokens.checked_add(tokens).ok_or("usage_overflow")?;
                target.unknown_tokens += unknown;
            }
        }
        by_id.insert(id, operation.operation);
    }
    report.execution_intervals = all.elapsed;
    report.execution_unknown = all.unknown_elapsed;
    report.agent_intervals = all.active;
    report.agent_unknown = all.unknown_active;
    add_coverage(
        connection,
        &mut report,
        &by_id,
        start_ms,
        end_ms,
        end_ms - start_ms,
        1,
    )?;
    Ok((report, groups))
}
