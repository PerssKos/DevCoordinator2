use super::{HostBackend, candidate};
use crate::storage::{
    blocked, fs,
    model::{Context, Discovery, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use devcoordinator2_executor_core::log_query::{
    LogPruneRequest, LogQueryError, RetentionObservation, discover_retention_run_step,
    discover_retention_step, remove_retained_run, retained_run_observations,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct EvidenceCursor {
    worktree_index: usize,
    current: Option<WorktreeEvidence>,
    incomplete: bool,
    complete: bool,
}

struct WorktreeEvidence {
    repository: String,
    worktree: PathBuf,
    request: LogPruneRequest,
    filesystem_cursor: Option<String>,
    filesystem_complete: bool,
    observations: VecDeque<RetentionObservation>,
    run: Option<MeasuredRun>,
    measurement: Option<(RetentionObservation, fs::MeasurementCursor)>,
    incomplete: bool,
}

struct MeasuredRun {
    run_id: String,
    identity: (u64, u64),
    test: String,
    finished_at_ms: Option<u64>,
    active: bool,
    leaves: u64,
    bytes: u64,
    device: Option<u64>,
    newest_modified_ns: i128,
    failed: bool,
}

impl WorktreeEvidence {
    fn finish_run(
        &mut self,
        backend: &HostBackend,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        let Some(run) = self.run.take() else {
            return Ok(());
        };
        if run.failed {
            self.incomplete = true;
            return Ok(());
        }
        let root = self
            .worktree
            .join(".devcoordinator/test/logs/runs")
            .join(&run.run_id);
        if fs::identity(&root).ok() != Some(run.identity) {
            self.incomplete = true;
            return Ok(());
        }
        let protected = match crate::storage::evidence::run_is_protected(
            &backend.database,
            &self.worktree,
            &run.run_id,
        ) {
            Ok(protected) => protected,
            Err(_) => {
                self.incomplete = true;
                return Ok(());
            }
        };
        let retention = crate::storage::evidence::run_retention_state(
            &backend.database,
            &self.worktree,
            &run.run_id,
            context.retention_age_seconds,
            context.retention_depth,
        );
        let published = match retention {
            Ok(Some(state)) if state.payload_leaves == run.leaves => Some(state),
            _ => {
                self.incomplete = true;
                None
            }
        };
        let mut record = candidate(
            api::Kind::Evidence,
            api::Effect::RetainedEvidence,
            run.test,
            Locator::Evidence {
                worktree: self.worktree.clone(),
                run_id: run.run_id.clone(),
                leaf: "run".into(),
            },
            context,
        )?;
        record.artifact.repository_id = Some(self.repository.clone());
        record.artifact.repository_name = context
            .repositories
            .iter()
            .find(|repository| repository.id == self.repository)
            .map(|repository| repository.name.clone());
        record.artifact.ownership = "retention_owned".into();
        record.artifact.last_used_at_ms = run.finished_at_ms;
        record.artifact.allocated_bytes = Some(run.bytes);
        record.artifact.filesystem_id = run.device.map(|device| format!("fs-{device}"));
        record.retention_eligible = published.as_ref().is_some_and(|state| state.eligible);
        record.last_activity_signature = format!(
            "{:?}:{}:{}:{}",
            run.finished_at_ms, run.leaves, run.bytes, run.newest_modified_ns
        );
        // One exact run root represents all payload descendants. A large run
        // keeps scalar totals instead of accumulating an unbounded alias list.
        record.private_aliases.push(root);
        if run.active || published.as_ref().is_some_and(|state| state.active) {
            record.blockers.push("active_evidence".into());
            record.retention_eligible = false;
        }
        if protected {
            record.blockers.push("required_evidence".into());
        }
        out.records.push(record);
        Ok(())
    }
}

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
        let mut cursor = None;
        loop {
            if let Some(absent) = self.evidence_absent_step(
                r,
                context,
                &mut cursor,
                super::DiscoveryBudget::default(),
            )? {
                return Ok(absent);
            }
        }
    }

    pub(super) fn evidence_absent_step(
        &self,
        record: &Record,
        context: &Context,
        cursor: &mut Option<String>,
        budget: super::DiscoveryBudget,
    ) -> Result<Option<bool>, ProtocolError> {
        let Locator::Evidence {
            worktree, run_id, ..
        } = &record.locator
        else {
            return Err(blocked("evidence_identity_invalid"));
        };
        let repository = record
            .artifact
            .repository_id
            .as_deref()
            .ok_or_else(|| blocked("ownership_unknown"))?;
        let query = request(worktree, repository, context)?;
        let result = discover_retention_run_step(
            worktree,
            &query,
            run_id,
            cursor.as_deref(),
            budget.entries.min(16),
            budget.elapsed,
        )
        .map_err(|_| unavailable("evidence_inventory_unavailable"))?;
        if !result.observations.is_empty() {
            *cursor = None;
            return Ok(Some(false));
        }
        *cursor = result.next_cursor;
        Ok(cursor.is_none().then_some(true))
    }

    pub(super) fn discover_evidence_step(
        &self,
        context: &Context,
        out: &mut Discovery,
        cursor: &mut EvidenceCursor,
        budget: super::DiscoveryBudget,
    ) -> Result<bool, ProtocolError> {
        if cursor.complete {
            return Ok(true);
        }
        if cursor.current.is_none() {
            let Some((repository, worktree)) = context.worktrees.get(cursor.worktree_index) else {
                if !cursor.incomplete {
                    out.complete_kinds.push(api::Kind::Evidence);
                }
                cursor.complete = true;
                return Ok(true);
            };
            if context
                .scan_repository_id
                .as_ref()
                .is_some_and(|id| id != repository)
            {
                cursor.worktree_index += 1;
                return Ok(false);
            }
            match request(worktree, repository, context) {
                Ok(query) => {
                    cursor.current = Some(WorktreeEvidence {
                        repository: repository.clone(),
                        worktree: worktree.clone(),
                        request: query,
                        filesystem_cursor: None,
                        filesystem_complete: false,
                        observations: VecDeque::new(),
                        run: None,
                        measurement: None,
                        incomplete: false,
                    })
                }
                Err(_) => {
                    cursor.incomplete = true;
                    cursor.worktree_index += 1;
                    out.coverage_gaps
                        .push("evidence_inventory_incomplete".into());
                }
            }
            return Ok(false);
        }
        let state = cursor.current.as_mut().expect("worktree initialized");
        if let Some((observation, measurement)) = state.measurement.as_mut() {
            match measurement.step(budget.entries, budget.elapsed) {
                Ok(Some(measured)) => {
                    let run = state.run.as_mut().expect("observed run initialized");
                    if (measured.device, measured.inode) != observation.leaf_identity
                        || run.device.is_some_and(|device| device != measured.device)
                    {
                        run.failed = true;
                        state.incomplete = true;
                    }
                    if let (Some(leaves), Some(bytes)) = (
                        run.leaves.checked_add(1),
                        run.bytes.checked_add(measured.bytes),
                    ) {
                        run.leaves = leaves;
                        run.bytes = bytes;
                    } else {
                        run.failed = true;
                        state.incomplete = true;
                    }
                    run.device = Some(measured.device);
                    run.newest_modified_ns =
                        run.newest_modified_ns.max(measured.newest_modified_ns);
                    run.active |= observation.active;
                    state.measurement = None;
                }
                Ok(None) => {}
                Err(_) => {
                    state.run.as_mut().expect("observed run initialized").failed = true;
                    state.incomplete = true;
                    state.measurement = None;
                }
            }
            return Ok(false);
        }
        if let Some(observation) = state.observations.front() {
            if state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id != observation.run_id)
            {
                state.finish_run(self, context, out)?;
                return Ok(false);
            }
            let observation = state
                .observations
                .pop_front()
                .expect("observation front exists");
            state.run.get_or_insert_with(|| MeasuredRun {
                run_id: observation.run_id.clone(),
                identity: observation.run_identity,
                test: observation.test.clone(),
                finished_at_ms: observation.run_finished_at_ms,
                active: observation.active,
                leaves: 0,
                bytes: 0,
                device: None,
                newest_modified_ns: 0,
                failed: false,
            });
            if state
                .run
                .as_ref()
                .is_some_and(|run| run.identity != observation.run_identity)
            {
                state.run.as_mut().expect("observed run initialized").failed = true;
                state.incomplete = true;
            }
            let path = observation.components.iter().fold(
                state.worktree.join(".devcoordinator/test/logs/runs"),
                |path, component| path.join(component),
            );
            match fs::MeasurementCursor::new(&path) {
                Ok(measurement) => state.measurement = Some((observation, measurement)),
                Err(_) => {
                    state.run.as_mut().expect("observed run initialized").failed = true;
                    state.incomplete = true;
                }
            }
            return Ok(false);
        }
        if state.filesystem_complete {
            if state.run.is_some() {
                state.finish_run(self, context, out)?;
            } else {
                cursor.incomplete |= state.incomplete;
                if state.incomplete {
                    out.coverage_gaps
                        .push("evidence_inventory_incomplete".into());
                }
                cursor.current = None;
                cursor.worktree_index += 1;
            }
            return Ok(false);
        }
        match discover_retention_step(
            &state.worktree,
            &state.request,
            state.filesystem_cursor.as_deref(),
            budget.entries.min(16),
            budget.elapsed,
        ) {
            Ok(discovered) => {
                state.filesystem_cursor = discovered.next_cursor;
                state.filesystem_complete = state.filesystem_cursor.is_none();
                state.observations = discovered.observations.into();
            }
            Err(LogQueryError::MaintenanceBusy) => {
                // Another retention/protection turn owns the worktree. Keep
                // this scan's exact continuation for the scheduler's retry.
                return Err(ProtocolError::new(
                    devcoordinator2_api::ErrorCode::Busy,
                    "evidence_retention_busy",
                ));
            }
            Err(_) => {
                state.incomplete = true;
                state.filesystem_complete = true;
                if let Some(run) = &mut state.run {
                    run.failed = true;
                }
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    pub(super) fn discover_evidence(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        let mut cursor = EvidenceCursor::default();
        while !self.discover_evidence_step(
            context,
            out,
            &mut cursor,
            super::DiscoveryBudget::default(),
        )? {}
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
        let observations = retained_run_observations(worktree, &query, run_id)
            .map_err(|_| unavailable("evidence_inventory_unavailable"))?;
        if observations.is_empty() {
            return Err(blocked("evidence_already_removed"));
        }
        if observations.iter().any(|observation| observation.active) {
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
        if !self.evidence_absent(r, context)? {
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

    struct NoHostDocker;
    impl crate::docker::DockerControl for NoHostDocker {
        fn invoke(
            &self,
            _: crate::docker::DockerInvocation,
        ) -> Result<crate::docker::DockerOutput, crate::docker::DockerError> {
            Err(crate::docker::DockerError::CliUnavailable)
        }
        fn spawn_follow_logs(
            &self,
            _: &crate::docker::ExactContainerId,
        ) -> Result<crate::docker::LogFollower, crate::docker::DockerError> {
            panic!("evidence discovery must not follow container logs")
        }
    }

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
            source_owner_observation: None,
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
            ignored_review: None,
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
        let sparse_evidence =
            first.join(".devcoordinator/test/logs/runs/run-one/checks/check/cases/case/evidence");
        std::fs::create_dir_all(&sparse_evidence).unwrap();
        std::fs::File::create(sparse_evidence.join("large.bin"))
            .unwrap()
            .set_len(512 * 1024 * 1024)
            .unwrap();
        // EventService owns an asynchronous dispatcher Database clone. This
        // integration owns its cluster explicitly so process exit cannot race
        // the dispatcher's final shared-fixture release.
        let cluster = devcoordinator2_persistence::fixture::FixtureCluster::start().unwrap();
        let database =
            Database::connect(&cluster.create_database("evidence_discovery").unwrap()).unwrap();
        let first_text = first.to_string_lossy().into_owned();
        let second_text = second.to_string_lossy().into_owned();
        let first_for_db = first.clone();
        let second_for_db = second.clone();
        let first_record = artifact_record(&first, "run-one");
        let second_record = artifact_record(&second, "run-two");
        database
            .transaction(move |c| {
                c.execute(
                    "INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',$1,'one','t',1,'t'),('r2222222222222222',$2,'two','t',1,'t')",
                    crate::database::params![first_text, second_text],
                )?;
                c.execute(
                    "INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1','r1111111111111111',$1,'t','t'),('w2','r2222222222222222',$2,'t','t')",
                    crate::database::params![
                        first_for_db.to_string_lossy(),
                        second_for_db.to_string_lossy()
                    ],
                )?;
                c.execute(
                    "INSERT INTO storage_artifacts(artifact_id,repository_id,kind,revision,removed_at_ms,record_json,updated_at_ms) VALUES('sa-run-one','r1111111111111111','evidence',1,NULL,$1,1),('sa-run-two','r2222222222222222','evidence',1,NULL,$2,1)",
                    crate::database::params![first_record, second_record],
                )?;
                Ok(())
            })
            .unwrap();

        let backend = std::sync::Arc::new(HostBackend::with_docker(
            config(temporary.path()),
            database.clone(),
            std::sync::Arc::new(NoHostDocker),
        ));
        let context = Context {
            worktrees: vec![
                ("r1111111111111111".into(), first.clone()),
                ("r2222222222222222".into(), second.clone()),
            ],
            repositories: vec![
                Repository {
                    id: "r1111111111111111".into(),
                    name: "one".into(),
                    root: first.clone(),
                    execution_uid: 1000,
                },
                Repository {
                    id: "r2222222222222222".into(),
                    name: "two".into(),
                    root: second.clone(),
                    execution_uid: 1000,
                },
            ],
            retention_age_seconds: 86_400,
            retention_depth: 3,
            ..Context::default()
        };
        // A fresh filesystem observation cannot claim complete policy
        // eligibility before retention has published a complete generation.
        let mut preliminary = Discovery::default();
        backend
            .discover_evidence(&context, &mut preliminary)
            .unwrap();
        assert_eq!(preliminary.records.len(), 2);
        assert!(!preliminary.complete_kinds.contains(&api::Kind::Evidence));
        let service = crate::storage::StorageService::with_backend(
            config(temporary.path()),
            database.clone(),
            std::sync::Arc::new(crate::platform::HostClock),
            crate::capacity::CapacityBroker::new(
                database.clone(),
                temporary.path().join("daemon.sock"),
            )
            .unwrap(),
            crate::events::EventService::new(database.clone()).unwrap(),
            backend.clone(),
        );
        let caller = crate::access::Caller {
            via_edge: false,
            pid: std::process::id(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Other,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: None,
        };
        let protection = || api::Protection {
            artifact_id: "sa-run-one".into(),
            expected_revision: 1,
            protected: true,
        };
        assert!(service.protect(protection(), &caller).is_err());
        let logs = crate::test_logs::TestLogService::new(
            database.clone(),
            crate::repository::Registry::new(database.clone()),
        );
        logs.set_retention(86_400, 3, "fixture", "2026-11-05T00:00:00Z")
            .unwrap();
        let report = logs.drain_maintenance_for_test().unwrap();
        assert!(report.errors.is_empty());
        let mut discovery = Discovery::default();
        let mut cursor = EvidenceCursor::default();
        let mut turns = 0;
        let budget = super::super::DiscoveryBudget {
            entries: 1,
            elapsed: std::time::Duration::from_millis(10),
        };
        // Contention after a real filesystem page must preserve both the
        // continuation and the partially measured run, rather than skip it.
        loop {
            assert!(
                !backend
                    .discover_evidence_step(&context, &mut discovery, &mut cursor, budget)
                    .unwrap()
            );
            turns += 1;
            assert!(turns < 100);
            if cursor.current.as_ref().is_some_and(|state| {
                state.filesystem_cursor.is_some()
                    && !state.filesystem_complete
                    && state.run.is_some()
                    && state.measurement.is_none()
                    && state.observations.is_empty()
            }) {
                break;
            }
        }
        let state = cursor.current.as_ref().unwrap();
        assert_eq!(state.worktree, first);
        let saved_cursor = state.filesystem_cursor.clone();
        let saved_leaves = state.run.as_ref().unwrap().leaves;
        let lock = devcoordinator2_executor_core::log_query::lock_retention(&first).unwrap();
        let busy = backend
            .discover_evidence_step(&context, &mut discovery, &mut cursor, budget)
            .expect_err("real retention contention must remain retryable");
        assert_eq!(busy.code, devcoordinator2_api::ErrorCode::Busy);
        let state = cursor.current.as_ref().unwrap();
        assert_eq!(cursor.worktree_index, 0);
        assert_eq!(state.filesystem_cursor, saved_cursor);
        assert_eq!(state.run.as_ref().unwrap().leaves, saved_leaves);
        assert!(!state.filesystem_complete);
        assert!(!state.incomplete);
        assert!(!state.run.as_ref().unwrap().failed);
        drop(lock);
        while !backend
            .discover_evidence_step(&context, &mut discovery, &mut cursor, budget)
            .unwrap()
        {
            turns += 1;
            assert!(turns < 200);
        }
        assert!(turns > 10);
        assert_eq!(discovery.records.len(), 2);
        assert!(
            discovery
                .records
                .iter()
                .all(|record| record.blockers == ["required_evidence"])
        );
        assert!(discovery.complete_kinds.contains(&api::Kind::Evidence));
        assert!(discovery.coverage_gaps.is_empty());
        assert!(
            discovery
                .records
                .iter()
                .all(|record| record.private_aliases.len() == 1)
        );
        assert!(
            discovery
                .records
                .iter()
                .all(|record| record.artifact.allocated_bytes.unwrap() < 1024 * 1024)
        );
        let pinned = service.protect(protection(), &caller).unwrap();
        assert!(pinned.protected);

        // A malformed sibling never participates in exact-run absence; a
        // removed leaf also cannot be pinned from an older published head.
        std::fs::create_dir_all(first.join(".devcoordinator/test/logs/runs/malformed-sibling"))
            .unwrap();
        let first_record = discovery
            .records
            .iter()
            .find(|record| record.artifact.repository_id.as_deref() == Some("r1111111111111111"))
            .unwrap();
        assert!(!backend.evidence_absent(first_record, &context).unwrap());
        let unpinned = service
            .protect(
                api::Protection {
                    expected_revision: pinned.revision,
                    protected: false,
                    ..protection()
                },
                &caller,
            )
            .unwrap();
        backend
            .validate_evidence(first_record, &context)
            .expect("an unrelated malformed run cannot block an exact cleanup target");
        let pinned = service
            .protect(
                api::Protection {
                    expected_revision: unpinned.revision,
                    ..protection()
                },
                &caller,
            )
            .unwrap();
        let leaf = first.join(".devcoordinator/test/logs/runs/run-one/checks/check/cases/case");
        std::fs::remove_dir_all(&leaf).unwrap();
        assert!(backend.evidence_absent(first_record, &context).unwrap());
        assert!(
            service
                .protect(
                    api::Protection {
                        expected_revision: pinned.revision,
                        ..protection()
                    },
                    &caller
                )
                .is_err()
        );

        // Exercise the actual accepted scan job and persisted scheduler
        // backoff too, without any host Docker operation or copied worker.
        std::fs::remove_dir(first.join(".devcoordinator/test/logs/runs/malformed-sibling"))
            .unwrap();
        publish_run(&first, "run-one");
        assert!(logs.drain_maintenance_for_test().unwrap().errors.is_empty());
        let scan = service
            .scan(
                api::Scan {
                    repository_id: None,
                    idempotency_key: "retention-lock-retry".into(),
                },
                &caller,
            )
            .unwrap();
        let lock = devcoordinator2_executor_core::log_query::lock_retention(&first).unwrap();
        let mut scan_cursor = None;
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 200);
            match service.execute_scan_step(&scan.job_id, &mut scan_cursor, budget) {
                Err(error) => {
                    assert_eq!(error.code, devcoordinator2_api::ErrorCode::Busy);
                    break;
                }
                Ok(false) => {}
                Ok(true) => panic!("scan completed while the evidence lock was held"),
            }
        }
        assert!(scan_cursor.is_some());
        let job = crate::storage::worker::scan_job_for_test(
            service.clone(),
            scan.job_id.clone(),
            scan_cursor,
        );
        let scheduler = crate::maintenance::Scheduler::new(database.clone(), 1, 2).unwrap();
        let job_budget = || {
            crate::maintenance::Budget::new(std::time::Duration::from_millis(25), 256, 256 * 1024)
        };
        let mut continuation = Some(scan.job_id.clone());
        assert!(matches!(
            scheduler
                .submit(job.clone(), job_budget(), continuation.clone())
                .unwrap()
                .status,
            crate::maintenance::JobStatus::Retry { .. }
        ));
        let key = format!("storage:{}", scan.job_id);
        database.read(|connection| {
            let (status,attempts,cursor):(String,u64,String)=connection.query_row("SELECT last_status,attempts,cursor::text FROM coordinator_maintenance WHERE job_key=$1",[&key],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            assert_eq!(status,"retry");
            assert_eq!(attempts,1);
            assert_eq!(serde_json::from_str::<String>(&cursor).unwrap(),scan.job_id);
            Ok(())
        }).unwrap();
        assert!(matches!(
            scheduler
                .submit(job.clone(), job_budget(), continuation.clone())
                .unwrap()
                .status,
            crate::maintenance::JobStatus::Deferred { .. }
        ));
        drop(lock);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut events = scheduler.retry_events();
        let (_stop, mut shutdown) = tokio::sync::watch::channel(false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline);
            events.borrow_and_update();
            match scheduler
                .submit(job.clone(), job_budget(), continuation.clone())
                .unwrap()
                .status
            {
                crate::maintenance::JobStatus::Complete => break,
                crate::maintenance::JobStatus::Continue { cursor } => continuation = Some(cursor),
                crate::maintenance::JobStatus::Deferred { not_before_ms } => {
                    assert!(runtime.block_on(scheduler.wait_retry(
                        &mut events,
                        Some(not_before_ms),
                        &mut shutdown
                    )))
                }
                status => panic!("unexpected scan retry outcome: {status:?}"),
            }
        }
        scheduler.shutdown_and_join();
        assert!(matches!(
            service.job(&scan.job_id).unwrap().state,
            api::JobState::Completed
        ));
        let page = service
            .inventory(api::List {
                kind: Some(api::Kind::Evidence),
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        // The two manually seeded protected artifacts remain conservative
        // history; require both actual native observations as well.
        assert_eq!(page.total, 4);
        let expected_ids = discovery
            .records
            .iter()
            .map(|record| record.artifact.artifact_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let observed = page
            .artifacts
            .iter()
            .filter(|artifact| expected_ids.contains(artifact.artifact_id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(observed.len(), 2);
        let repositories = observed
            .iter()
            .filter_map(|artifact| artifact.repository_id.as_deref())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            repositories,
            std::collections::BTreeSet::from(["r1111111111111111", "r2222222222222222"])
        );
        assert_eq!(
            database
                .read(|connection| Ok(connection.query_row(
                    "SELECT count(*) FROM coordinator_maintenance WHERE job_key=$1",
                    [&key],
                    |row| row.get::<_, u64>(0)
                )?))
                .unwrap(),
            0
        );
        service
            .protect(
                api::Protection {
                    expected_revision: pinned.revision,
                    protected: false,
                    ..protection()
                },
                &caller,
            )
            .unwrap();
        let malformed = first.join(".devcoordinator/test/logs/runs/malformed-sibling");
        std::fs::create_dir(&malformed).unwrap();
        backend.validate_evidence(first_record, &context).unwrap();
        backend.remove_evidence(first_record, &context).unwrap();
        assert!(backend.evidence_absent(first_record, &context).unwrap());
        assert!(malformed.is_dir());
        assert!(
            second
                .join(".devcoordinator/test/logs/runs/run-two/checks/check/cases/case/stdout.log")
                .is_file()
        );
        drop(runtime);
        drop(scheduler);
        drop(job);
        drop(logs);
        drop(service);
        drop(backend);
        drop(database);
        let mut admin = cluster.client("postgres").unwrap();
        admin
            .batch_execute("SET statement_timeout='500ms'; SET lock_timeout='100ms'")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname='evidence_discovery' AND backend_type='client backend'",
                &[],
            )
            .unwrap()
            .get::<_, i64>(0)
            != 0
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(admin);
        cluster.stop().unwrap();
    }
}
