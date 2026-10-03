use super::{HostBackend, candidate};
use crate::storage::{
    blocked, fs,
    model::{Context, Discovery, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use devcoordinator2_executor_core::log_query::{
    LogPruneRequest, LogQueryError, remove_retained_run, retention_runs,
};
use std::path::Path;

fn request(
    worktree: &Path,
    repository: &str,
    context: &Context,
) -> Result<LogPruneRequest, ProtocolError> {
    let active = crate::test_logs::active_run_id(worktree)
        .map_err(|_| unavailable("evidence_active_state_unavailable"))?;
    Ok(LogPruneRequest {
        schema: 2,
        repository_id: repository.into(),
        max_age_seconds: context.retention_age_seconds,
        case_depth: context.retention_depth,
        active_run_id: active,
    })
}

impl HostBackend {
    pub(super) fn evidence_absent(
        &self,
        r: &Record,
        context: &Context,
    ) -> Result<bool, ProtocolError> {
        let Locator::Evidence {
            worktree, run_id, ..
        } = &r.locator
        else {
            return Err(blocked("evidence_identity_invalid"));
        };
        let repository = r
            .artifact
            .repository_id
            .as_deref()
            .ok_or_else(|| blocked("ownership_unknown"))?;
        let query = request(worktree, repository, context)?;
        let runs = retention_runs(worktree, &query)
            .map_err(|_| unavailable("evidence_inventory_unavailable"))?;
        Ok(!runs
            .iter()
            .any(|run| run.run_id == *run_id && !run.payload_paths.is_empty()))
    }

    pub(super) fn discover_evidence(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        let mut complete = true;
        for (repository, worktree) in &context.worktrees {
            if context
                .scan_repository_id
                .as_ref()
                .is_some_and(|id| id != repository)
            {
                continue;
            }
            let observed = (|| {
                let query = request(worktree, repository, context)?;
                let protected = crate::storage::evidence::protected_runs(&self.database, worktree)?;
                let runs = retention_runs(worktree, &query)
                    .map_err(|_| unavailable("evidence_inventory_unavailable"))?;
                for run in runs {
                    let mut r = candidate(
                        api::Kind::Evidence,
                        api::Effect::RetainedEvidence,
                        run.test.clone(),
                        Locator::Evidence {
                            worktree: worktree.clone(),
                            run_id: run.run_id.clone(),
                            leaf: "run".into(),
                        },
                        context,
                    )?;
                    r.artifact.repository_id = Some(repository.clone());
                    r.artifact.repository_name = context
                        .repositories
                        .iter()
                        .find(|p| &p.id == repository)
                        .map(|p| p.name.clone());
                    r.artifact.ownership = "retention_owned".into();
                    r.artifact.last_used_at_ms = run.finished_at_ms;
                    r.retention_eligible = run.eligible;
                    r.last_activity_signature =
                        format!("{:?}:{}", run.finished_at_ms, run.payload_paths.len());
                    let base = worktree.join(".devcoordinator/test/logs/runs");
                    let mut size = 0;
                    for relative in &run.payload_paths {
                        let path = base.join(relative);
                        let m = fs::measure(&path)?;
                        size += m.bytes;
                        r.private_aliases.push(path);
                        r.artifact.filesystem_id = Some(format!("fs-{}", m.device));
                    }
                    r.artifact.allocated_bytes = Some(size);
                    if run.active {
                        r.blockers.push("active_evidence".into());
                    }
                    if protected.contains(&run.run_id) {
                        r.blockers.push("required_evidence".into());
                    }
                    out.records.push(r);
                }
                Ok::<_, ProtocolError>(())
            })();
            if observed.is_err() {
                complete = false;
                out.coverage_gaps
                    .push("evidence_inventory_incomplete".into());
            }
        }
        if complete {
            out.complete_kinds.push(api::Kind::Evidence);
        }
        Ok(())
    }

    pub(super) fn validate_evidence(
        &self,
        r: &Record,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let Locator::Evidence {
            worktree, run_id, ..
        } = &r.locator
        else {
            return Err(blocked("evidence_identity_invalid"));
        };
        if crate::storage::evidence::protected_runs(&self.database, worktree)?.contains(run_id) {
            return Err(blocked("required_evidence"));
        }
        let repository = r
            .artifact
            .repository_id
            .as_deref()
            .ok_or_else(|| blocked("ownership_unknown"))?;
        let query = request(worktree, repository, context)?;
        let runs = retention_runs(worktree, &query)
            .map_err(|_| unavailable("evidence_inventory_unavailable"))?;
        let run = runs
            .iter()
            .find(|run| &run.run_id == run_id)
            .ok_or_else(|| blocked("evidence_already_removed"))?;
        if run.active {
            return Err(blocked("active_evidence"));
        }
        Ok(())
    }

    pub(super) fn remove_evidence(
        &self,
        r: &Record,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let Locator::Evidence {
            worktree, run_id, ..
        } = &r.locator
        else {
            return Err(blocked("evidence_identity_invalid"));
        };
        let repository = r
            .artifact
            .repository_id
            .as_deref()
            .ok_or_else(|| blocked("ownership_unknown"))?;
        let query = request(worktree, repository, context)?;
        remove_retained_run(
            worktree,
            query.clone(),
            || {
                crate::storage::evidence::protected_runs(&self.database, worktree)
                    .map_err(|_| LogQueryError::Unavailable)
            },
            run_id,
        )
        .map_err(|_| unavailable("evidence_removal_incomplete"))?;
        if retention_runs(worktree, &query)
            .map_err(|_| unavailable("evidence_inventory_unavailable"))?
            .iter()
            .any(|run| &run.run_id == run_id)
        {
            return Err(blocked("evidence_still_retained"));
        }
        Ok(())
    }
}
