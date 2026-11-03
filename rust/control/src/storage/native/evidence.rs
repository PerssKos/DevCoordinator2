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
        // Protection metadata is global to the scan. Build it once, then use
        // the exact worktree mapping for each retained-run observation. If the
        // snapshot cannot be read, preserve the existing fail-closed behavior
        // by reporting an incomplete evidence inventory instead of discovering
        // evidence without its protection blockers.
        let protected_snapshot = match crate::storage::evidence::protection_snapshot(&self.database)
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                out.coverage_gaps
                    .push("evidence_inventory_incomplete".into());
                return Ok(());
            }
        };
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
                let protected = protected_snapshot.get(worktree);
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
                    if protected.is_some_and(|runs| runs.contains(&run.run_id)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::database::Database;
    use crate::storage::model::{Context, Discovery, Repository};
    use devcoordinator2_executor_core::{
        LeafLogMetadata, LeafSelector, RunLogLease, RunLogMetadata,
    };
    use devcoordinator2_executor_protocol::{
        DiagnosticExit, LeafStatus, LogPhase, LogStream, RunStatus,
    };
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn config(root: &Path) -> Config {
        Config {
            socket_path: root.join("daemon.sock"),
            sandbox_bridge_dir: root.join("bridge"),
            state_dir: root.join("state"),
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "fixture".into(),
            port_range: (20_000, 20_010),
            base_domain: "fixture.invalid".into(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://fixture.invalid".into(),
            bugs_dir: root.join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        }
    }

    fn publish_run(worktree: &Path, run_id: &str) {
        let run_dir = worktree.join(".devcoordinator/test/logs/runs").join(run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let lease = RunLogLease::acquire(&run_dir, run_id).unwrap();
        lease
            .publish_run_metadata(&RunLogMetadata {
                schema: 2,
                run_id: run_id.into(),
                test: "fixture".into(),
                started_at_epoch_ms: 1,
                finished_at_epoch_ms: Some(2),
                status: RunStatus::Passed,
                complete: true,
            })
            .unwrap();
        let selector =
            LeafSelector::new(Some("check".into()), LogPhase::Case, Some("case".into())).unwrap();
        for (stream, payload) in [
            (LogStream::Stdout, b"fixture evidence\n".as_slice()),
            (LogStream::Stderr, b"fixture diagnostics\n".as_slice()),
        ] {
            let mut writer = lease.create_stream(selector.clone(), stream).unwrap();
            writer.write_all(payload).unwrap();
            writer.seal().unwrap();
        }
        lease
            .publish_leaf_metadata(&LeafLogMetadata {
                schema: 2,
                selector,
                status: LeafStatus::Passed,
                exit: DiagnosticExit {
                    code: Some(0),
                    signal: None,
                },
                started_at_epoch_ms: 1,
                finished_at_epoch_ms: Some(2),
                process_started: true,
                complete: true,
                structured_evidence_formats: Vec::new(),
                structured_evidence_count: 0,
            })
            .unwrap();
    }

    fn artifact_record(path: &Path, run_id: &str) -> String {
        let repository_id = if run_id == "run-one" {
            "r1111111111111111"
        } else {
            "r2222222222222222"
        };
        let artifact = devcoordinator2_api::storage::Artifact {
            artifact_id: format!("sa-{run_id}"),
            revision: 1,
            repository_id: Some(repository_id.into()),
            repository_name: Some("fixture".into()),
            group_id: None,
            group_name: None,
            accounting_id: format!("sa-{run_id}"),
            name: run_id.into(),
            kind: devcoordinator2_api::storage::Kind::Evidence,
            effect: devcoordinator2_api::storage::Effect::RetainedEvidence,
            ownership: "retention_owned".into(),
            filesystem_id: None,
            allocated_bytes: Some(1),
            last_used_at_ms: None,
            observed_since_ms: 1,
            verified_at_ms: Some(1),
            eligible_at_ms: None,
            safety: devcoordinator2_api::storage::Safety::Protected,
            reasons: Vec::new(),
            protected: true,
            deletable: false,
            automatic_eligible: false,
            dependencies: Vec::new(),
            aliases: Vec::new(),
            removed_at_ms: None,
        };
        serde_json::to_string(&crate::storage::model::Record {
            update_sequence: 1,
            artifact,
            locator: Locator::Evidence {
                worktree: path.to_owned(),
                run_id: run_id.into(),
                leaf: "run".into(),
            },
            resource_key: format!("evidence:{run_id}"),
            ancestor_keys: Vec::new(),
            fingerprint: format!("fingerprint:{run_id}"),
            owner_deployment: None,
            last_activity_signature: String::new(),
            discovered_effect: devcoordinator2_api::storage::Effect::RetainedEvidence,
            disposal_approved: false,
            disposal_reason: None,
            blockers: Vec::new(),
            recovery_lineage: None,
            recovery_verified: false,
            recovery_created_at_ms: None,
            private_aliases: Vec::new(),
            retention_eligible: true,
        })
        .unwrap()
    }

    #[test]
    fn native_discovery_maps_snapshot_across_multiple_worktrees() {
        let temporary = TempDir::new().unwrap();
        let first = temporary.path().join("repo-one");
        let second = temporary.path().join("repo-two");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        publish_run(&first, "run-one");
        publish_run(&second, "run-two");
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let first_text = first.to_string_lossy().into_owned();
        let second_text = second.to_string_lossy().into_owned();
        let first_for_db = first.clone();
        let second_for_db = second.clone();
        let first_record = artifact_record(&first, "run-one");
        let second_record = artifact_record(&second, "run-two");
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',?1,'one','t',1,'t'),('r2222222222222222',?2,'two','t',1,'t')",
                    rusqlite::params![first_text, second_text],
                )?;
                c.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1','r1111111111111111',?1,'t','t'),('w2','r2222222222222222',?2,'t','t')",
                    rusqlite::params![
                        first_for_db.to_string_lossy(),
                        second_for_db.to_string_lossy()
                    ],
                )?;
                c.execute(
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-one','r1111111111111111','evidence',1,NULL,?1,1),('sa-run-two','r2222222222222222','evidence',1,NULL,?2,1)",
                    rusqlite::params![first_record, second_record],
                )?;
                Ok(())
            })
            .unwrap();

        let backend = HostBackend::new(config(temporary.path()), database);
        let context = Context {
            worktrees: vec![
                ("r1111111111111111".into(), first.clone()),
                ("r2222222222222222".into(), second.clone()),
            ],
            repositories: vec![
                Repository {
                    id: "r1111111111111111".into(),
                    name: "one".into(),
                    root: first,
                    execution_uid: 1000,
                },
                Repository {
                    id: "r2222222222222222".into(),
                    name: "two".into(),
                    root: second,
                    execution_uid: 1000,
                },
            ],
            retention_age_seconds: 86_400,
            retention_depth: 3,
            ..Context::default()
        };
        let mut discovery = Discovery::default();
        backend.discover_evidence(&context, &mut discovery).unwrap();
        assert_eq!(discovery.records.len(), 2);
        assert!(
            discovery
                .records
                .iter()
                .all(|record| record.blockers == ["required_evidence"])
        );
        assert!(discovery.complete_kinds.contains(&api::Kind::Evidence));
        assert!(discovery.coverage_gaps.is_empty());
    }
}
