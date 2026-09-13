use devcoordinator2_api::review::{Disposition, EvidenceKind, ObservationKind, ReviewRecord};
use devcoordinator2_api::{ErrorCode, ProtocolError};

use crate::database::DatabaseError;

pub(crate) fn invalid(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, message)
}

pub(crate) fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        _ => ProtocolError::new(
            ErrorCode::InternalError,
            "Review/evidence database operation failed",
        ),
    }
}

pub(crate) fn page(offset: u32, limit: u8) -> Result<(), ProtocolError> {
    if !(1..=10).contains(&limit) || offset > 100_000 {
        return Err(invalid("Page limit must be 1..10 and offset 0..100000"));
    }
    Ok(())
}

pub(crate) fn window(start: u64, end: u64) -> Result<(), ProtocolError> {
    if end <= start || end > i64::MAX as u64 {
        return Err(invalid(
            "Review window must be nonempty and within Unix-ms range",
        ));
    }
    Ok(())
}

pub(crate) fn text(value: &str, minimum: usize, maximum: usize) -> Result<(), ProtocolError> {
    if value.trim().len() < minimum
        || value.len() > maximum
        || value.chars().any(|character| character.is_control())
    {
        return Err(invalid(
            "Review/evidence text is empty, oversized, or contains control characters",
        ));
    }
    Ok(())
}

pub(crate) fn record(record: &ReviewRecord) -> Result<(), ProtocolError> {
    window(record.window_start_ms, record.window_end_ms)?;
    if record.version != 1 || record.repository_id != record.project_id {
        return Err(invalid(
            "Review version must be 1 and projectId must equal repositoryId",
        ));
    }
    text(&record.repository_id, 1, 100)?;
    if let Some(workstream) = &record.workstream_id {
        text(workstream, 1, 100)?;
    }
    let experiment = &record.experiment;
    if let Some(comparison) = &experiment.comparison {
        text(&comparison.narrative, 40, 800)?;
        if let Some(reason) = &comparison.input_change_reason {
            text(reason, 24, 800)?;
        }
    }
    if !experiment.preserves_quality || experiment.scope_repo_id != record.repository_id {
        return Err(invalid(
            "Optimization must preserve quality and remain in the reviewed repository",
        ));
    }
    for value in [
        &experiment.hypothesis,
        &experiment.chosen_action,
        &experiment.success_criteria,
        &experiment.rollback_condition,
        &experiment.reason,
        &experiment.baseline.interpretation,
    ] {
        text(value, 16, 800)?;
    }
    if !(2..=5).contains(&experiment.alternatives.len())
        || experiment
            .alternatives
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != experiment.alternatives.len()
    {
        return Err(invalid("Provide two to five distinct alternatives"));
    }
    for alternative in &experiment.alternatives {
        text(alternative, 8, 400)?;
    }
    if experiment.baseline.missing_measurements.len() > 20 || experiment.observations.len() > 8 {
        return Err(invalid(
            "Review measurements or observations exceed their bounds",
        ));
    }
    for gap in &experiment.baseline.missing_measurements {
        text(gap, 1, 160)?;
    }
    let mut lists = vec![
        &experiment.evidence_refs,
        &experiment.result_evidence_refs,
        &experiment.baseline.evidence_refs,
    ];
    for observation in &experiment.observations {
        text(&observation.interpretation, 24, 800)?;
        if observation.evidence_refs.is_empty() {
            return Err(invalid("Interpretations require evidence references"));
        }
        lists.push(&observation.evidence_refs);
    }
    for refs in lists {
        if refs.len() > 12 {
            return Err(invalid("At most twelve references per evidence list"));
        }
        for evidence in refs {
            text(&evidence.reference, 1, 240)?;
        }
    }
    if !experiment
        .baseline
        .evidence_refs
        .iter()
        .any(|evidence| evidence.kind == EvidenceKind::Usage)
    {
        return Err(invalid(
            "Baseline must reference canonical usage, including unavailable measurements",
        ));
    }
    if experiment.disposition != Disposition::Proposed {
        if experiment.observations.is_empty()
            || !experiment
                .evidence_refs
                .iter()
                .any(|evidence| evidence.kind == EvidenceKind::Decision)
            || !experiment
                .observations
                .iter()
                .flat_map(|observation| &observation.evidence_refs)
                .any(|evidence| {
                    matches!(
                        evidence.kind,
                        EvidenceKind::Outcome | EvidenceKind::Run | EvidenceKind::Release
                    )
                })
        {
            return Err(invalid(
                "A review cannot finalize from totals alone: link standing decisions and interpreted outcome/run evidence",
            ));
        }
        if matches!(
            experiment.disposition,
            Disposition::Applied | Disposition::Retained
        ) && !experiment
            .observations
            .iter()
            .any(|observation| observation.kind == ObservationKind::AvoidableWork)
        {
            return Err(invalid(
                "User waiting or intentional validation alone does not justify an optimization",
            ));
        }
        if matches!(
            experiment.disposition,
            Disposition::Retained | Disposition::Reverted
        ) && (experiment.comparison.is_none()
            || !experiment
                .result_evidence_refs
                .iter()
                .any(|evidence| evidence.kind == EvidenceKind::Usage)
            || !experiment
                .result_evidence_refs
                .iter()
                .any(|evidence| matches!(evidence.kind, EvidenceKind::Run | EvidenceKind::Release)))
        {
            return Err(invalid(
                "Retained or reverted actions require a comparison narrative, post-baseline canonical measurements and quality evidence; otherwise use inconclusive",
            ));
        }
        if experiment.disposition == Disposition::Unchanged {
            text(&experiment.reason, 40, 800)?;
        }
    }
    Ok(())
}
