use super::*;
use devcoordinator2_api::results::TestStatus;
use devcoordinator2_api::results::UsageCoverage;

#[derive(Clone, Debug, PartialEq)]
struct InputIdentity {
    workload: String,
    source: String,
    configuration: String,
}

impl ReviewService {
    pub(super) fn validate_review_evidence(
        &self,
        record: &ReviewRecord,
        prepared: &Prepared,
        now_ms: u64,
        deadline: Instant,
    ) -> Result<(), ProtocolError> {
        let experiment = &record.experiment;
        let concluded = matches!(
            experiment.disposition,
            Disposition::Retained | Disposition::Reverted
        );
        let expected = usage_ref(
            &record.repository_id,
            record.window_start_ms,
            record.window_end_ms,
        );
        let mut baseline_inputs = Vec::new();
        for reference in &experiment.baseline.evidence_refs {
            if reference.kind == EvidenceKind::Usage {
                if reference != &expected {
                    return Err(invalid("Baseline usage must match the exact review window"));
                }
            } else if let Some(identity) = self.validate_reference(
                &record.repository_id,
                reference.clone(),
                EvidenceUse::Baseline {
                    end_ms: record.window_end_ms,
                },
            )? {
                baseline_inputs.push(identity);
            }
        }
        let mut context = experiment
            .evidence_refs
            .iter()
            .chain(
                experiment
                    .observations
                    .iter()
                    .flat_map(|observation| &observation.evidence_refs),
            )
            .cloned()
            .collect::<Vec<_>>();
        if let Some(outcome) = &record.outcome_id {
            context.push(EvidenceRef {
                kind: EvidenceKind::Outcome,
                reference: outcome.clone(),
            });
        }
        for reference in context {
            if reference.kind == EvidenceKind::Usage {
                if reference != expected && !experiment.result_evidence_refs.contains(&reference) {
                    return Err(invalid(
                        "Usage citations must name the baseline or a declared result window",
                    ));
                }
            } else {
                self.validate_reference(&record.repository_id, reference, EvidenceUse::Context)?;
            }
        }
        let repository = self.repository(&record.repository_id)?;
        let mut measured_result = false;
        let mut seen_usage = std::collections::HashSet::new();
        let mut result_inputs = Vec::new();
        for reference in &experiment.result_evidence_refs {
            if reference.kind == EvidenceKind::Usage {
                let (start, end) = result_window(&reference.reference, record, now_ms)?;
                if !seen_usage.insert(reference.reference.clone()) {
                    continue;
                }
                let usage = self
                    .usage
                    .review_window(&repository, start, end, deadline)?;
                measured_result |= measured(&usage.coverage);
                if usage.coverage.has_gaps {
                    require_gap(record, "result_usage_partial_or_unavailable")?;
                }
                if usage.totals.total_tokens.is_none() {
                    require_gap(record, "result_total_tokens_unavailable")?;
                }
                if usage
                    .coverage
                    .unavailable_reasons
                    .contains_key("query_budget_exhausted")
                {
                    require_gap(record, "result_usage_query_budget_exhausted")?;
                }
                if usage.time.summed_agent_active.unknown_intervals > 0 {
                    require_gap(record, "result_active_time_partial")?;
                }
            } else {
                let purpose = if concluded {
                    EvidenceUse::Result {
                        start_ms: record.window_end_ms,
                        end_ms: now_ms,
                    }
                } else {
                    EvidenceUse::Context
                };
                if let Some(identity) =
                    self.validate_reference(&record.repository_id, reference.clone(), purpose)?
                {
                    result_inputs.push(identity);
                }
            }
        }
        if concluded {
            if !measured(&prepared.usage.coverage) || !measured_result {
                return Err(invalid(
                    "Retained/reverted needs actual before and after canonical measurements; otherwise use inconclusive",
                ));
            }
            if baseline_inputs.is_empty() || result_inputs.is_empty() {
                require_gap(record, "comparison_input_identity_unavailable")?;
            } else {
                let pairs = baseline_inputs
                    .iter()
                    .flat_map(|before| result_inputs.iter().map(move |after| (before, after)))
                    .filter(|(before, after)| before.workload == after.workload)
                    .collect::<Vec<_>>();
                if pairs.is_empty() {
                    return Err(invalid(
                        "Result evidence covers a different workload or target; use inconclusive",
                    ));
                }
                if !pairs.iter().any(|(before, after)| before == after)
                    && experiment
                        .comparison
                        .as_ref()
                        .and_then(|comparison| comparison.input_change_reason.as_ref())
                        .is_none()
                {
                    return Err(invalid(
                        "Explain the observed source/configuration change in comparison.inputChangeReason or use inconclusive",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_reference(
        &self,
        repository_id: &str,
        reference: EvidenceRef,
        purpose: EvidenceUse,
    ) -> Result<Option<InputIdentity>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database.call(move |connection| {
            match reference.kind {
                EvidenceKind::Usage => Err(invalid("Unexpected usage reference").into()),
                EvidenceKind::Outcome | EvidenceKind::Decision => {
                    let query = if reference.kind == EvidenceKind::Outcome {
                        "SELECT EXISTS(SELECT 1 FROM tasks WHERE repository_id=?1 AND task_id=?2)"
                    } else {
                        "SELECT EXISTS(SELECT 1 FROM decisions WHERE repository_id=?1 AND (decision_id=?2 OR ref=?2))"
                    };
                    let exists: bool = connection.query_row(query, rusqlite::params![repository_id, reference.reference], |row| row.get(0))?;
                    if !exists { return Err(invalid("Evidence is missing or belongs to another repository").into()); }
                    Ok(None)
                }
                EvidenceKind::Release => {
                    let encoded = connection.query_row("SELECT receipt_json FROM release_evidence WHERE repository_id=?1 AND receipt_id=?2", rusqlite::params![repository_id,reference.reference], |row| row.get::<_, String>(0)).optional()?
                        .ok_or_else(|| invalid("Release evidence is missing or belongs to another repository"))?;
                    let receipt: devcoordinator2_api::delivery::Receipt = serde_json::from_str(&encoded).map_err(|_| invalid("Invalid delivery receipt"))?;
                    check_time(purpose, receipt.verified_at_ms)?;
                    if matches!(purpose, EvidenceUse::Result { .. })
                        && (!receipt.qualified || receipt.qualification != devcoordinator2_api::delivery::Qualification::Qualified || receipt.verified_at_ms != receipt.delivered_at_ms) {
                        return Err(invalid("Result delivery is not qualified").into());
                    }
                    identity(format!("release:{:?}:{}", receipt.kind, receipt.target), receipt.source_sha256, receipt.config_sha256).map(Some).map_err(Into::into)
                }
                EvidenceKind::Run => {
                    let (worktree, run_id) = reference.reference.split_once('/').ok_or_else(|| invalid("Run reference must be worktree_id/run_id"))?;
                    let path = connection.query_row("SELECT worktree_path FROM worktrees WHERE repository_id=?1 AND worktree_id=?2", rusqlite::params![repository_id,worktree], |row| row.get::<_, String>(0)).optional()?
                        .ok_or_else(|| invalid("Run worktree belongs to another repository"))?;
                    let history = TestRunStore.read_history(std::path::Path::new(&path)).map_err(|_| invalid("Run history unavailable"))?;
                    let run = history.iter().find(|run| run.run_id == run_id).ok_or_else(|| invalid("Run evidence unavailable"))?;
                    check_time(purpose, run.finished_at.as_deref().and_then(unix_ms))?;
                    if matches!(purpose, EvidenceUse::Result { .. }) && run.status != TestStatus::Passed { return Err(invalid("Result run must pass").into()); }
                    let metadata = TestRunStore.read_evidence(std::path::Path::new(&path)).map_err(|_| invalid("Run input evidence unavailable"))?;
                    let Some(metadata) = metadata.into_iter().find(|metadata| metadata.run_id == run_id) else { return Ok(None); };
                    if metadata.test != run.test || (matches!(purpose, EvidenceUse::Result { .. }) && metadata.status != devcoordinator2_executor_protocol::RunStatus::Passed) {
                        return Err(invalid("Run history and retained input evidence disagree").into());
                    }
                    identity(format!("run:{}", run.test), metadata.source_digest, metadata.config_digest).map(Some).map_err(Into::into)
                }
            }
        }).map_err(database_error)
    }
}

fn check_time(purpose: EvidenceUse, observed: Option<u64>) -> Result<(), ProtocolError> {
    let valid = match purpose {
        EvidenceUse::Context => true,
        EvidenceUse::Baseline { end_ms } => observed.is_some_and(|observed| observed <= end_ms),
        EvidenceUse::Result { start_ms, end_ms } => {
            observed.is_some_and(|observed| observed >= start_ms && observed <= end_ms)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "Quality evidence timestamp is missing or outside the required baseline/result window",
        ))
    }
}

fn result_window(
    reference: &str,
    record: &ReviewRecord,
    now_ms: u64,
) -> Result<(u64, u64), ProtocolError> {
    let prefix = format!("canonical-usage:{}:", record.repository_id);
    let (start, end) = reference.strip_prefix(&prefix).and_then(|window| window.split_once(':')).ok_or_else(|| invalid("Result usage must reference the same repository and explicit start/end milliseconds"))?;
    let start = start
        .parse::<u64>()
        .map_err(|_| invalid("Invalid result usage start"))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| invalid("Invalid result usage end"))?;
    window(start, end)?;
    if start < record.window_end_ms
        || end > now_ms
        || usage_ref(&record.repository_id, start, end).reference != reference
    {
        return Err(invalid(
            "Result usage must be a canonical post-baseline window ending no later than now",
        ));
    }
    Ok((start, end))
}

fn measured(coverage: &UsageCoverage) -> bool {
    coverage.available_collectors > 0 && coverage.contributing_collectors > 0
}

fn require_gap(record: &ReviewRecord, gap: &str) -> Result<(), ProtocolError> {
    if record
        .experiment
        .baseline
        .missing_measurements
        .iter()
        .any(|known| known == gap)
    {
        Ok(())
    } else {
        Err(invalid(&format!(
            "Acknowledge coverage gap {gap} in baseline.missingMeasurements"
        )))
    }
}

fn identity(
    workload: String,
    source: String,
    configuration: String,
) -> Result<InputIdentity, ProtocolError> {
    if [&source, &configuration]
        .iter()
        .any(|digest| digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(invalid("Run source/configuration identity is invalid"));
    }
    Ok(InputIdentity {
        workload,
        source,
        configuration,
    })
}
