use super::{
    StorageService, blocked, conflict, db_error, hash, json,
    model::{Context, Discovery, Repository, atom},
    parse,
};
use crate::{access::Caller, database::DatabaseError, events::NewEvent};
use devcoordinator2_api::{
    ClientKind, ProtocolError,
    results::{OtherOwnedEvent, OwnedEvent},
    storage as api,
};
use rusqlite::OptionalExtension;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;

impl StorageService {
    fn has_intent(&self, job: &str, record: &super::model::Record) -> Result<bool, ProtocolError> {
        let job = job.to_owned();
        let record = record.clone();
        self.database.call(move |c| {
            let intent = c.query_row("SELECT fingerprint,revision FROM storage_item_intents WHERE job_id=?1 AND artifact_id=?2",
                rusqlite::params![job,record.artifact.artifact_id], |r| Ok((r.get::<_,String>(0)?,super::super_sql_u64(r.get::<_,i64>(1)?)?))).optional()?;
            match intent {
                None => Ok(false),
                Some((fingerprint, revision)) if fingerprint == record.fingerprint && revision == record.artifact.revision => Ok(true),
                _ => Err(DatabaseError::Domain(conflict("cleanup_intent_changed"))),
            }
        }).map_err(db_error)
    }

    fn persist_intent(
        &self,
        job: &str,
        record: &super::model::Record,
    ) -> Result<(), ProtocolError> {
        let job = job.to_owned();
        let record = record.clone();
        let now = self.now_ms();
        self.database
            .transaction(move |c| {
                let state: String = c.query_row(
                    "SELECT state FROM storage_jobs WHERE job_id=?1",
                    [&job],
                    |r| r.get(0),
                )?;
                if state != "running" {
                    return Err(DatabaseError::Domain(blocked("cleanup_cancelled")));
                }
                let revision: i64 = c.query_row(
                    "SELECT revision FROM storage_artifacts WHERE artifact_id=?1",
                    [&record.artifact.artifact_id],
                    |r| r.get(0),
                )?;
                if revision != record.artifact.revision as i64 {
                    return Err(DatabaseError::Domain(conflict("artifact_changed")));
                }
                c.execute(
                    "INSERT OR IGNORE INTO storage_item_intents VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![
                        job,
                        record.artifact.artifact_id,
                        record.fingerprint,
                        { revision },
                        now as i64
                    ],
                )?;
                Ok(())
            })
            .map_err(db_error)
    }

    fn commit_item(
        &self,
        job_id: &str,
        mut record: super::model::Record,
        receipt: api::ItemReceipt,
        added: u64,
    ) -> Result<api::Job, ProtocolError> {
        let job_id = job_id.to_owned();
        let now = self.now_ms();
        self.database.transaction(move|c|{
            record.update_sequence = super::next_sequence(c)?;
            let value=c.query_row("SELECT job_json FROM storage_jobs WHERE job_id=?1",[&job_id],|r|r.get::<_,String>(0))?;
            let mut job:api::Job=parse(&value).map_err(DatabaseError::Domain)?;
            let step=job.receipts.len() as u32;
            let changed=c.execute("UPDATE storage_artifacts SET removed_at_ms=?1,revision=?2,record_json=?3,updated_at_ms=?4 WHERE artifact_id=?5 AND revision=?6",rusqlite::params![record.artifact.removed_at_ms.map(|n|n as i64),record.artifact.revision as i64,json(&record).map_err(DatabaseError::Domain)?,now as i64,record.artifact.artifact_id,(record.artifact.revision-1) as i64])?;
            if changed != 1 { return Err(DatabaseError::Domain(conflict("artifact_changed"))); }
            c.execute("INSERT INTO storage_item_receipts VALUES(?1,?2,?3,?4)",rusqlite::params![job_id,receipt.artifact_id,step,json(&receipt).map_err(DatabaseError::Domain)?])?;
            job.receipts.push(receipt);job.reclaimed_bytes=job.reclaimed_bytes.saturating_add(added);
            if job.receipts.last().is_some_and(|r| matches!(r.status.as_str(),"removed"|"partial") && r.space_change.is_none() && r.code.as_deref()!=Some("removed_shared_reference")) && record.artifact.kind != api::Kind::Mount {
                job.unmeasured_items += 1;
            }
            // Preserve a cancellation committed while the native step was running.
            c.execute("UPDATE storage_jobs SET job_json=?1 WHERE job_id=?2",rusqlite::params![json(&job).map_err(DatabaseError::Domain)?,job_id])?;
            Ok(job)
        }).map_err(db_error)
    }

    fn begin_job(&self, id: &str) -> Result<Option<api::Job>, ProtocolError> {
        let id = id.to_owned();
        let now = self.now_ms();
        self.database
            .transaction(move |c| {
                let value = c.query_row(
                    "SELECT job_json FROM storage_jobs WHERE job_id=?1",
                    [&id],
                    |r| r.get::<_, String>(0),
                )?;
                let mut job: api::Job = parse(&value).map_err(DatabaseError::Domain)?;
                if matches!(
                    job.state,
                    api::JobState::Cancelling | api::JobState::Cancelled
                ) {
                    job.state = api::JobState::Cancelled;
                    job.completed_at_ms.get_or_insert(now);
                    c.execute(
                        "UPDATE storage_jobs SET state='cancelled',job_json=?1 WHERE job_id=?2",
                        rusqlite::params![json(&job).map_err(DatabaseError::Domain)?, id],
                    )?;
                    return Ok(None);
                }
                if !matches!(job.state, api::JobState::Queued) {
                    return Ok(None);
                }
                job.state = api::JobState::Running;
                job.started_at_ms.get_or_insert(now);
                c.execute(
                    "UPDATE storage_jobs SET state='running',job_json=?1 WHERE job_id=?2",
                    rusqlite::params![json(&job).map_err(DatabaseError::Domain)?, id],
                )?;
                Ok(Some(job))
            })
            .map_err(db_error)
    }

    pub(crate) fn context(&self) -> Result<Context, ProtocolError> {
        let now = self.now_ms();
        let (repositories,worktrees,deployments,generations,components,observed,containers,leases)=self.database.call(move|c|{
            let repositories={let mut q=c.prepare("SELECT repository_id,display_name,root_path,registered_by_uid FROM repositories ORDER BY repository_id")?;q.query_map([],|r|Ok(Repository {id:r.get(0)?,name:r.get(1)?,root:PathBuf::from(r.get::<_,String>(2)?),execution_uid:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?};
            let worktrees={let mut q=c.prepare("SELECT repository_id,worktree_path FROM worktrees ORDER BY worktree_id")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,PathBuf::from(r.get::<_,String>(1)?))))?.collect::<Result<Vec<_>,_>>()?};
            let deployments={let mut q=c.prepare("SELECT d.deployment_id,d.repository_id,d.name,d.source,d.spec_json,w.worktree_path FROM deployments d JOIN worktrees w ON w.worktree_id=d.worktree_id")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,PathBuf::from(r.get::<_,String>(5)?))))?.collect::<Result<Vec<_>,_>>()?};
            let generations={let mut q=c.prepare("SELECT g.deployment_id,g.path FROM generations g JOIN deployments d ON d.deployment_id=g.deployment_id WHERE g.number=d.current_generation OR g.number=d.previous_generation")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,PathBuf::from(r.get::<_,String>(1)?))))?.collect::<Result<Vec<_>,_>>()?};
            let components={let mut q=c.prepare("SELECT deployment_id,binding_kind,binding_identity FROM components WHERE binding_identity IS NOT NULL")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?)))?.collect::<Result<Vec<_>,_>>()?};
            let observed={let mut q=c.prepare("SELECT observed_deployment_id,repository_id,native_project FROM observed_deployments")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,(r.get::<_,String>(1)?,r.get::<_,String>(2)?))))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let containers={let mut q=c.prepare("SELECT container_id,repository_id,observed_deployment_id FROM observed_containers")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,(r.get::<_,String>(1)?,r.get::<_,String>(2)?))))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let leases={let mut q=c.prepare("SELECT lease_json FROM storage_leases WHERE expires_at_ms>?1")?;q.query_map([now as i64],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
            Ok((repositories,worktrees,deployments,generations,components,observed,containers,leases))
        }).map_err(db_error)?;
        let mut context = Context {
            repositories,
            worktrees,
            roots: self.root_records()?,
            observed,
            recorded_containers: containers,
            now_ms: now,
            ..Context::default()
        };
        // Git also knows linked worktrees that have never run a Coordinator
        // command. Inspect it under the registered repository owner's identity.
        for repository in context.repositories.clone() {
            if let Ok(bytes) = super::native::git_bytes(
                &repository.root,
                &["worktree", "list", "--porcelain", "-z"],
                &context,
            ) {
                use std::os::unix::ffi::OsStringExt;
                for field in bytes.split(|b| *b == 0) {
                    if let Some(path) = field.strip_prefix(b"worktree ") {
                        let path = PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
                        if path.is_absolute()
                            && !context
                                .worktrees
                                .iter()
                                .any(|(id, p)| id == &repository.id && p == &path)
                        {
                            context.worktrees.push((repository.id.clone(), path));
                        }
                    }
                }
            }
        }
        for record in self
            .records()?
            .into_values()
            .filter(|r| r.artifact.protected && r.artifact.removed_at_ms.is_none())
        {
            context.protected_resources.push(record.resource_key);
            context.protected_ancestors.extend(record.ancestor_keys);
        }
        let (age,depth)=self.database.call(|c|c.query_row("SELECT max_age_seconds,case_depth FROM test_log_retention_state WHERE singleton=1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).map_err(DatabaseError::from)).map_err(db_error)?;
        context.retention_age_seconds = age.max(1) as u64;
        context.retention_depth = depth.max(1) as u64;
        for (_, path) in &context.worktrees {
            if !matches!(crate::test_logs::active_run_id(path), Ok(None)) {
                context.active_worktrees.push(path.clone());
            }
        }
        for (id, repo, name, source, spec, path) in deployments {
            // Missing or unreadable declarations do not prove retirement.
            let current = crate::repository_config::list_deployment_names(&path)
                .map(|names| names.contains(&name))
                .unwrap_or(true);
            if !current {
                continue;
            }
            context.deployment_repositories.insert(id.clone(), repo);
            context.deployment_names.insert(id.clone(), name);
            if source == "worktree" {
                context.current_paths.push(path.clone());
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&spec) {
                collect_declared(&v, &path, &mut context);
            }
        }
        for (id, path) in generations {
            if context.deployment_repositories.contains_key(&id) {
                context.current_paths.push(path);
            }
        }
        for (id, kind, binding) in components {
            if context.deployment_repositories.contains_key(&id) {
                match kind.as_deref() {
                    Some("container") => context.current_container_ids.push(binding),
                    Some("compose") => {
                        context.current_projects.insert(binding, id);
                    }
                    _ => {}
                }
            }
        }
        for value in leases {
            let lease: api::Lease = parse(&value)?;
            context.leased_artifacts.extend(lease.artifact_ids);
        }
        for record in self
            .records()?
            .into_values()
            .filter(|r| context.leased_artifacts.contains(&r.artifact.artifact_id))
        {
            context.leased_resources.push(record.resource_key);
            context.leased_ancestors.extend(record.ancestor_keys);
        }
        context.current_paths.sort();
        context.current_paths.dedup();
        Ok(context)
    }

    pub fn register_legacy(
        &self,
        p: api::LegacyRegister,
        caller: &Caller,
    ) -> Result<api::Job, ProtocolError> {
        super::text(&p.reason, 2000)?;
        let revision = self
            .inventory(api::List {
                limit: Some(1),
                ..Default::default()
            })?
            .revision;
        if revision != p.expected_inventory_revision {
            return Err(conflict("inventory_changed"));
        }
        let key = format!(
            "{}-{}-{}",
            p.deployment_id,
            p.expected_inventory_revision,
            &hash(p.reason.as_bytes())[..16]
        );
        self.queue("legacy_registration", &key, &p, caller, None)
    }

    pub async fn serve(&self, mut shutdown: watch::Receiver<bool>) {
        let _ = self.recover_jobs();
        let mut workers = tokio::task::JoinSet::new();
        let mut active = BTreeSet::new();
        let mut next_scan = self.now_ms();
        let mut event_cursor = None;
        let mut event_retry = Duration::from_secs(1);
        let mut rescan_requested = false;
        loop {
            if *shutdown.borrow() {
                break;
            }
            if self
                .discovery_requested
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                next_scan = next_scan.min(self.now_ms().saturating_add(1000));
            }
            if self.now_ms() >= next_scan {
                let now = self.now_ms();
                let caller = system_caller();
                let scanning = self.database.call(|c| c.query_row("SELECT EXISTS(SELECT 1 FROM storage_jobs WHERE kind='scan' AND state IN ('queued','running') AND json_extract(request_json,'$.repository_id') IS NULL)",[],|r|r.get::<_,bool>(0)).map_err(DatabaseError::from)).unwrap_or(true);
                if scanning {
                    rescan_requested = true;
                } else {
                    let _ = self.scan(
                        api::Scan {
                            repository_id: None,
                            idempotency_key: format!(
                                "scheduled-{now}-{}",
                                event_cursor.unwrap_or(0)
                            ),
                        },
                        &caller,
                    );
                }
                next_scan = now.saturating_add(3_600_000);
            }
            // The observation window and lease end are deadlines, not merely
            // hints for the next hourly scan. Deletion still follows a fresh scan.
            if let Ok(deadline) = self.next_eligibility_deadline()
                && let Some(deadline) = deadline
            {
                next_scan = next_scan.min(deadline);
            }
            if let Ok(jobs) = self.pending_jobs() {
                for (job, uid) in jobs {
                    if !active.insert(job.job_id.clone()) {
                        continue;
                    }
                    let service = self.clone();
                    let signal = shutdown.clone();
                    let id = job.job_id.clone();
                    workers.spawn(async move {
                        service.run_job(job, uid, signal).await;
                        (id, ())
                    });
                }
            }
            let delay = Duration::from_millis(
                next_scan
                    .saturating_sub(self.now_ms())
                    .clamp(100, 3_600_000),
            );
            let subscription = self.events.subscribe(
                devcoordinator2_api::params::EventWait {
                    cursor: event_cursor,
                    limit: 100,
                    filters: vec![devcoordinator2_api::params::EventFilter {
                        filter_id: "storage-lifecycle".into(),
                        categories: vec![
                            devcoordinator2_api::params::EventCategory::Test,
                            devcoordinator2_api::params::EventCategory::Deployment,
                        ],
                        kinds: Vec::new(),
                        repository_ids: Vec::new(),
                        deployment_ids: Vec::new(),
                        deadline_at: Some(
                            (time::OffsetDateTime::now_utc()
                                + time::Duration::milliseconds(delay.as_millis() as i64))
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                        ),
                    }],
                },
                crate::events::EventVisibility::unrestricted(),
            );
            let retry_delay = event_retry;
            event_retry = if subscription.is_ok() {
                Duration::from_secs(1)
            } else {
                (event_retry * 2).min(Duration::from_secs(30))
            };
            let lifecycle = async {
                match subscription {
                    Ok(subscription) => subscription.receive().await.ok(),
                    // Only a denied/unavailable event subscription uses this
                    // service-owned bounded retry. A timeout never means success.
                    Err(_) => {
                        let _ = tokio::time::timeout(retry_delay, self.wake.notified()).await;
                        None
                    }
                }
            };
            tokio::select! {
                _=self.wake.notified()=>{},
                // The event deadline is a wakeup, not proof that the storage
                // clock advanced. Recheck next_scan at the top of the loop.
                event=lifecycle=>{if let Some(event)=event {event_cursor=Some(event.cursor);if !event.events.is_empty(){next_scan=next_scan.min(self.now_ms().saturating_add(1000));}}},
                result=workers.join_next(),if !workers.is_empty()=>{if let Some(Ok((id,())))=result{active.remove(&id);if rescan_requested {next_scan=self.now_ms();rescan_requested=false;}}},
                changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break;}},
            }
        }
        while workers.join_next().await.is_some() {}
    }

    async fn run_job(&self, mut job: api::Job, uid: u32, shutdown: watch::Receiver<bool>) {
        let run = format!("storage-{}", job.job_id);
        if self.capacity.register_run(&run, uid).is_err() {
            return;
        }
        let admitted = self
            .capacity
            .admit_native(&run, "storage", uid, shutdown.clone())
            .await;
        if let Ok(permit) = admitted {
            if *shutdown.borrow() {
                drop(permit);
                let _ = self.capacity.unregister_run(&run);
                return;
            }
            if let Ok(Some(current)) = self.begin_job(&job.job_id) {
                job = current;
                self.publish(&job, "started");
                let service = self.clone();
                let id = job.job_id.clone();
                let result = tokio::task::spawn_blocking(move || service.execute_job(&id)).await;
                if !matches!(result, Ok(Ok(()))) {
                    let mut latest = self.job(&job.job_id).unwrap_or(job.clone());
                    latest.state = if latest.receipts.iter().any(|r| r.status == "removed") {
                        api::JobState::Partial
                    } else {
                        api::JobState::Failed
                    };
                    latest.error_code = Some(match result {
                        Ok(Err(e)) => e.message,
                        _ => "storage_worker_interrupted".into(),
                    });
                    let _ = self.finish(&latest.job_id, latest.state, latest.error_code);
                }
            }
            drop(permit);
        }
        let _ = self.capacity.unregister_run(&run);
        self.wake.notify_waiters();
    }

    /// Finite execution boundary used by the native worker and acceptance fixtures.
    pub fn execute_job(&self, id: &str) -> Result<(), ProtocolError> {
        let job = self.job(id)?;
        let key = id.to_owned();
        let raw = self
            .database
            .call(move |c| {
                c.query_row(
                    "SELECT request_json FROM storage_jobs WHERE job_id=?1",
                    [key],
                    |r| r.get::<_, String>(0),
                )
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        match job.kind.as_str() {
            "scan" => {
                let request: api::Scan = parse(&raw)?;
                let sequence = self
                    .database
                    .transaction(super::next_sequence)
                    .map_err(db_error)?;
                let mut context = self.context()?;
                context.scan_repository_id = request.repository_id;
                let result = match self.backend.discover(&context) {
                    Ok(result) => result,
                    Err(error) => {
                        self.merge_discovery(
                            Discovery {
                                repository_id: context.scan_repository_id.clone(),
                                coverage_gaps: vec!["discovery_failed".into()],
                                ..Discovery::default()
                            },
                            sequence,
                        )?;
                        return Err(error);
                    }
                };
                #[cfg(feature = "root-acceptance")]
                if self
                    .config
                    .unit_prefix
                    .starts_with("devcoordinator2-rustint-")
                {
                    let barrier = self.config.state_dir.join("storage-pause-scan");
                    if std::fs::read_to_string(&barrier).ok().as_deref()
                        == Some(request.idempotency_key.as_str())
                    {
                        std::fs::write(barrier.with_extension("ready"), id)
                            .map_err(|_| blocked("fixture_scan_barrier_unavailable"))?;
                        let deadline = std::time::Instant::now() + Duration::from_secs(90);
                        while barrier.exists() {
                            if std::time::Instant::now() >= deadline {
                                return Err(blocked("fixture_scan_barrier_timeout"));
                            }
                            std::thread::sleep(Duration::from_millis(25));
                        }
                    }
                }
                self.merge_discovery(result, sequence)?;
                self.finish(id, api::JobState::Completed, None)?;
                self.queue_due_cleanup()?;
            }
            "legacy_registration" => {
                let request: api::LegacyRegister = parse(&raw)?;
                let context = self.context()?;
                let records = self.records()?.into_values().collect::<Vec<_>>();
                let ids = self
                    .backend
                    .legacy_group(&request.deployment_id, &records, &context)?;
                let mut owned = ids
                    .iter()
                    .map(|id| self.record(id))
                    .collect::<Result<Vec<_>, _>>()?;
                let _resources = self.lock_resources(&job.job_id, &owned)?;
                let context = self.context()?;
                for r in &owned {
                    self.backend.validate(r, &context, &ids)?;
                }
                for r in &mut owned {
                    r.disposal_approved = true;
                    r.disposal_reason = Some(request.reason.clone());
                    r.artifact.ownership = "cleanup_owned".into();
                    r.blockers
                        .retain(|s| s != "disposal_not_authorized" && s != "ownership_unknown");
                    r.artifact.revision += 1;
                }
                let actor = job.actor.clone();
                let now = self.now_ms();
                self.database.transaction(move |c| {
                    let sequence = super::next_sequence(c)?;
                    for mut r in owned {
                        r.update_sequence = sequence;
                        let id = &r.artifact.artifact_id;
                        let changed = c.execute("UPDATE storage_artifacts SET revision=?1,record_json=?2,updated_at_ms=?3 WHERE artifact_id=?4 AND revision=?5",rusqlite::params![r.artifact.revision as i64,json(&r).map_err(DatabaseError::Domain)?,now as i64,id,(r.artifact.revision-1) as i64])?;
                        if changed != 1 { return Err(DatabaseError::Domain(conflict("artifact_changed"))); }
                        super::change(c,Some(id),"legacy_disposal",&actor,now,"{}")?;
                    }
                    Ok(())
                }).map_err(db_error)?;
                self.finish(id, api::JobState::Completed, None)?;
            }
            "cleanup" => self.execute_cleanup(&job)?,
            _ => return Err(blocked("unknown_storage_job")),
        }
        Ok(())
    }

    fn execute_cleanup(&self, job: &api::Job) -> Result<(), ProtocolError> {
        let plan = self.stored_plan(
            job.plan_id
                .as_deref()
                .ok_or_else(|| blocked("cleanup_plan_missing"))?,
        )?;
        let selected = plan
            .records
            .iter()
            .map(|r| r.artifact.artifact_id.clone())
            .collect::<Vec<_>>();
        let _resources = self.lock_resources(&job.job_id, &plan.records)?;
        let mut failed = BTreeSet::new();
        let prior = self.job(&job.job_id)?;
        // Retain the whole group's private definitions before the first
        // destructive step. Existing files are immutable and reused on restart.
        for record in &plan.records {
            let state = self.job(&job.job_id)?.state;
            if matches!(state, api::JobState::Cancelling | api::JobState::Cancelled) {
                return self.finish(&job.job_id, api::JobState::Cancelled, None);
            }
            if !prior
                .receipts
                .iter()
                .any(|r| r.artifact_id == record.artifact.artifact_id && r.status == "removed")
            {
                self.backend.save_recovery(record, &job.job_id)?;
            }
        }
        let mut completed_resources = BTreeSet::new();
        for receipt in &prior.receipts {
            if receipt.status != "removed" {
                failed.insert(receipt.artifact_id.clone());
            } else if let Some(r) = plan
                .records
                .iter()
                .find(|r| r.artifact.artifact_id == receipt.artifact_id)
            {
                completed_resources.insert(r.resource_key.clone());
            }
        }
        for original in &plan.records {
            let latest = self.job(&job.job_id)?;
            if matches!(
                latest.state,
                api::JobState::Cancelling | api::JobState::Cancelled
            ) {
                return self.finish(&job.job_id, api::JobState::Cancelled, None);
            }
            let id = &original.artifact.artifact_id;
            if latest.receipts.iter().any(|r| r.artifact_id == *id) {
                continue;
            }
            let record = self.record(id)?;
            let mut attempted = false;
            let mut shared_removed = false;
            let mut space_before = None;
            let outcome = (|| {
                if original
                    .artifact
                    .dependencies
                    .iter()
                    .any(|id| failed.contains(id))
                {
                    return Err(blocked("dependency_failed"));
                }
                if original.fingerprint != record.fingerprint
                    || original.resource_key != record.resource_key
                    || original.artifact.revision != record.artifact.revision
                {
                    return Err(conflict("artifact_changed"));
                }
                let context = self.context()?;
                if (completed_resources.contains(&record.resource_key)
                    || record
                        .ancestor_keys
                        .iter()
                        .any(|key| completed_resources.contains(key)))
                    && self.backend.absent(&record, &context)?
                {
                    shared_removed = true;
                    return Ok(());
                }
                let resuming = self.has_intent(&job.job_id, &record)?;
                if resuming {
                    if self.backend.absent(&record, &context)? {
                        return Ok(());
                    }
                    if matches!(record.locator, super::model::Locator::Mount { .. }) {
                        attempted = true;
                        self.backend
                            .resume(&record, &context, &selected, &job.job_id)?;
                        if !self.backend.absent(&record, &context)? {
                            return Err(blocked("removal_unverified"));
                        }
                        return Ok(());
                    }
                }
                for (scope, revision) in &plan.policies {
                    if self
                        .policy(api::Scope {
                            repository_id: (!scope.is_empty()).then_some(scope.clone()),
                        })?
                        .revision
                        != *revision
                    {
                        return Err(conflict("policy_changed"));
                    }
                }
                if plan.public.automatic
                    && !self
                        .project_with(record.clone(), &self.projection()?)?
                        .automatic_eligible
                {
                    return Err(blocked("automatic_policy_not_due"));
                }
                self.backend.validate(&record, &context, &selected)?;
                self.persist_intent(&job.job_id, &record)?;
                self.backend.save_recovery(&record, &job.job_id)?;
                // Probe a surviving parent on the resource's actual filesystem.
                // A bind-backed Docker volume therefore measures the backing
                // filesystem, rather than counting its data as freed at unmount.
                space_before = record
                    .private_aliases
                    .iter()
                    .filter_map(|p| p.parent())
                    .chain(record.private_aliases.iter().map(|p| p.as_path()))
                    .find_map(|parent| {
                        super::fs::filesystem(parent, self.now_ms(), "Storage")
                            .ok()
                            .filter(|f| {
                                record.artifact.filesystem_id.as_ref() == Some(&f.filesystem_id)
                            })
                            .map(|f| (parent.to_path_buf(), f))
                    });
                attempted = true;
                self.backend
                    .remove(&record, &context, &selected, &job.job_id)?;
                #[cfg(feature = "root-acceptance")]
                if self
                    .config
                    .unit_prefix
                    .starts_with("devcoordinator2-rustint-")
                    && std::fs::remove_file(
                        self.config.state_dir.join("storage-crash-after-remove"),
                    )
                    .is_ok()
                {
                    // Deliberately stop an isolated acceptance daemon between
                    // the real side effect and its database receipt.
                    std::process::abort();
                }
                if !self.backend.absent(&record, &context)? {
                    return Err(blocked("removal_unverified"));
                }
                Ok(())
            })();
            let now = self.now_ms();
            let space_change = space_before.and_then(|(path, before)| {
                super::fs::filesystem(&path, now, "Storage")
                    .ok()
                    .filter(|after| after.filesystem_id == before.filesystem_id)
                    .map(|after| api::SpaceChange {
                        filesystem_id: before.filesystem_id,
                        available_before: before.available_bytes,
                        available_after: after.available_bytes,
                        measured_at_ms: now,
                    })
            });
            let mut updated = record.clone();
            updated.artifact.revision += 1;
            let receipt = match outcome {
                Ok(()) => {
                    updated.artifact.removed_at_ms = Some(now);
                    updated.artifact.deletable = false;
                    updated.artifact.automatic_eligible = false;
                    api::ItemReceipt {
                        artifact_id: id.clone(),
                        status: "removed".into(),
                        code: shared_removed.then(|| "removed_shared_reference".into()),
                        measured_bytes_before: record.artifact.allocated_bytes,
                        removed_at_ms: Some(now),
                        space_change: space_change.clone(),
                    }
                }
                Err(error) => {
                    failed.insert(id.clone());
                    updated.blockers.push(error.message.clone());
                    updated.artifact.verified_at_ms = None;
                    api::ItemReceipt {
                        artifact_id: id.clone(),
                        status: if error.message == "cleanup_cancelled" {
                            "cancelled"
                        } else if attempted {
                            "partial"
                        } else {
                            "blocked"
                        }
                        .into(),
                        code: Some(error.message),
                        measured_bytes_before: record.artifact.allocated_bytes,
                        removed_at_ms: None,
                        space_change: space_change.clone(),
                    }
                }
            };
            let has_backing = record.artifact.kind == api::Kind::Volume
                && plan.records.iter().any(|r| {
                    r.artifact.kind == api::Kind::BackingDirectory
                        && r.resource_key == record.resource_key
                });
            let allocation_bound = if has_backing {
                0
            } else {
                record.artifact.allocated_bytes.unwrap_or(0)
            };
            let added = space_change
                .map(|space| {
                    space
                        .available_after
                        .saturating_sub(space.available_before)
                        .min(allocation_bound)
                })
                .unwrap_or(0);
            if receipt.status == "removed" {
                completed_resources.insert(record.resource_key.clone());
            }
            let current = self.commit_item(&job.job_id, updated, receipt, added)?;
            self.publish(&current, "progress");
        }
        let latest = self.job(&job.job_id)?;
        let state = if failed.is_empty() {
            api::JobState::Completed
        } else if latest
            .receipts
            .iter()
            .any(|r| matches!(r.status.as_str(), "removed" | "partial"))
        {
            api::JobState::Partial
        } else {
            api::JobState::Failed
        };
        self.finish(
            &job.job_id,
            state,
            (!failed.is_empty()).then(|| "cleanup_has_blocked_items".into()),
        )
    }

    fn merge_discovery(
        &self,
        mut discovery: Discovery,
        sequence: u64,
    ) -> Result<(), ProtocolError> {
        let now = self.now_ms();
        let existing = self.records()?;
        let context = self.context()?;
        // A slow scan may finish after a newer scan, protection change or
        // cleanup. Its observations cannot replace those later facts, even
        // when an unchanged scan did not advance the public artifact revision.
        discovery.records.retain(|r| {
            existing
                .get(&r.artifact.artifact_id)
                .is_none_or(|old| old.update_sequence < sequence)
        });
        let mut verified_missing = BTreeSet::new();
        let mut unavailable = BTreeSet::new();
        for (id, old) in &existing {
            if old.artifact.removed_at_ms.is_none()
                && old.update_sequence < sequence
                && (old.artifact.kind != api::Kind::BuildCache || discovery.repository_id.is_none())
                && discovery
                    .repository_id
                    .as_ref()
                    .is_none_or(|repo| old.artifact.repository_id.as_ref() == Some(repo))
                && !discovery
                    .records
                    .iter()
                    .any(|r| r.artifact.artifact_id == *id)
                && !discovery.complete_kinds.contains(&old.artifact.kind)
            {
                unavailable.insert(id.clone());
            }
            if old.artifact.removed_at_ms.is_none()
                && old.update_sequence < sequence
                && discovery.complete_kinds.contains(&old.artifact.kind)
                && !discovery
                    .records
                    .iter()
                    .any(|r| r.artifact.artifact_id == *id)
                && discovery
                    .repository_id
                    .as_ref()
                    .is_none_or(|repo| old.artifact.repository_id.as_ref() == Some(repo))
                && self.backend.absent(old, &context).unwrap_or(false)
            {
                verified_missing.insert(id.clone());
            }
        }
        let mut seen = BTreeSet::new();
        for r in &mut discovery.records {
            r.update_sequence = sequence;
            if r.blockers.iter().any(|reason| {
                matches!(
                    reason.as_str(),
                    "active_process"
                        | "active_consumer"
                        | "active_worktree"
                        | "active_evidence"
                        | "active_lease"
                )
            }) {
                r.artifact.last_used_at_ms = Some(now);
            }
            seen.insert(r.artifact.artifact_id.clone());
            if let Some(old) = existing.get(&r.artifact.artifact_id) {
                if old.artifact.removed_at_ms.is_some() || old.resource_key != r.resource_key {
                    r.artifact.revision = old.artifact.revision + 1;
                    r.artifact.observed_since_ms = now;
                    r.artifact.protected = old.artifact.protected;
                    // A replacement never inherits a past disposal declaration,
                    // even when a provider reuses a name or filesystem identity.
                    continue;
                }
                r.artifact.observed_since_ms = old.artifact.observed_since_ms;
                r.artifact.protected = old.artifact.protected;
                r.disposal_approved = old.disposal_approved;
                r.disposal_reason = old.disposal_reason.clone();
                if old.disposal_approved {
                    r.artifact.effect = old.artifact.effect;
                    r.artifact.ownership = "cleanup_owned".into();
                    r.owner_deployment =
                        r.owner_deployment.clone().or(old.owner_deployment.clone());
                    r.artifact.repository_id = r
                        .artifact
                        .repository_id
                        .clone()
                        .or(old.artifact.repository_id.clone());
                    r.artifact.repository_name = r
                        .artifact
                        .repository_name
                        .clone()
                        .or(old.artifact.repository_name.clone());
                    r.blockers
                        .retain(|s| s != "ownership_unknown" && s != "disposal_not_authorized");
                }
                if old.last_activity_signature != r.last_activity_signature {
                    r.artifact.last_used_at_ms = Some(now);
                } else {
                    r.artifact.last_used_at_ms =
                        r.artifact.last_used_at_ms.max(old.artifact.last_used_at_ms);
                }
                let changed = old.fingerprint != r.fingerprint
                    || old.ancestor_keys != r.ancestor_keys
                    || old.last_activity_signature != r.last_activity_signature
                    || old.blockers != r.blockers
                    || old.artifact.dependencies != r.artifact.dependencies;
                r.artifact.revision = old.artifact.revision + u64::from(changed);
            }
        }
        let mut recovery: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, r) in discovery.records.iter().enumerate() {
            if r.recovery_verified
                && let Some(lineage) = &r.recovery_lineage
            {
                recovery.entry(lineage.clone()).or_default().push(i);
            }
        }
        for rows in recovery.values_mut() {
            rows.sort_by_key(|i| std::cmp::Reverse(discovery.records[*i].recovery_created_at_ms));
            let policy = self.policy(api::Scope {
                repository_id: discovery.records[rows[0]].artifact.repository_id.clone(),
            })?;
            for i in rows.iter().take(policy.minimum_verified_backups as usize) {
                discovery.records[*i]
                    .blockers
                    .push("required_recovery".into());
            }
        }
        let public_filesystems = json(&discovery.filesystems)?;
        let coverage = json(&discovery.coverage_gaps)?;
        let values = discovery
            .records
            .into_iter()
            .map(|r| {
                let expected = existing
                    .get(&r.artifact.artifact_id)
                    .map(|old| old.artifact.revision);
                json(&r).map(|s| (r, s, expected))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.database.transaction(move|c|{
            for (r,value,expected) in values {
                if super::locks::ensure_editable(c, &r.resource_key, None).is_err() { continue; }
                c.execute("INSERT INTO storage_artifacts VALUES(?1,?2,?3,?4,NULL,?5,?6) ON CONFLICT(artifact_id) DO UPDATE SET repository_id=excluded.repository_id,kind=excluded.kind,revision=excluded.revision,removed_at_ms=NULL,record_json=excluded.record_json,updated_at_ms=excluded.updated_at_ms WHERE storage_artifacts.revision=?7 AND COALESCE(json_extract(storage_artifacts.record_json,'$.update_sequence'),0) < ?8",rusqlite::params![r.artifact.artifact_id,r.artifact.repository_id,atom(r.artifact.kind),r.artifact.revision as i64,value,now as i64,expected.map(|v|v as i64),sequence as i64])?;
            }
            for (id,mut old) in existing {if !seen.contains(&id)&&verified_missing.contains(&id){
                if super::locks::ensure_editable(c, &old.resource_key, None).is_err() { continue; }
                old.artifact.removed_at_ms=Some(now);old.artifact.revision+=1;old.update_sequence=sequence;c.execute("UPDATE storage_artifacts SET removed_at_ms=?1,revision=?2,record_json=?3,updated_at_ms=?1 WHERE artifact_id=?4 AND revision=?5 AND COALESCE(json_extract(record_json,'$.update_sequence'),0) < ?6",rusqlite::params![now as i64,old.artifact.revision as i64,json(&old).map_err(DatabaseError::Domain)?,id,(old.artifact.revision-1) as i64,sequence as i64])?;
            }else if unavailable.contains(&id){
                if super::locks::ensure_editable(c,&old.resource_key,None).is_err(){continue;}
                old.artifact.verified_at_ms=None;old.artifact.revision+=1;old.update_sequence=sequence;
                if !old.blockers.iter().any(|r|r=="observation_unavailable"){old.blockers.push("observation_unavailable".into());}
                c.execute("UPDATE storage_artifacts SET revision=?1,record_json=?2,updated_at_ms=?3 WHERE artifact_id=?4 AND revision=?5 AND COALESCE(json_extract(record_json,'$.update_sequence'),0) < ?6",rusqlite::params![old.artifact.revision as i64,json(&old).map_err(DatabaseError::Domain)?,now as i64,id,(old.artifact.revision-1) as i64,sequence as i64])?;
            }}
            c.execute("UPDATE storage_scan_state SET last_scan_at_ms=?1,filesystem_json=?2,coverage_json=?3 WHERE singleton=1 AND revision=?4",rusqlite::params![now as i64,public_filesystems,coverage,sequence as i64])?;Ok(())
        }).map_err(db_error)
    }

    fn queue_due_cleanup(&self) -> Result<(), ProtocolError> {
        let projection = self.projection()?;
        let mut due = Vec::new();
        for record in self.records()?.into_values() {
            if self
                .project_with(record.clone(), &projection)?
                .automatic_eligible
            {
                let priority = match record.artifact.kind {
                    api::Kind::BackingDirectory => 0,
                    api::Kind::Volume => 1,
                    _ => 2,
                };
                due.push((priority, record.artifact.artifact_id));
            }
        }
        due.sort();
        let pending = self.database.call(|c| {
            let mut q=c.prepare("SELECT p.plan_json FROM storage_jobs j JOIN storage_plans p ON p.plan_id=json_extract(j.job_json,'$.plan_id') WHERE j.state IN ('queued','running','cancelling')")?;
            Ok(q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(db_error)?;
        let mut claimed = BTreeSet::new();
        for value in pending {
            let plan: super::model::StoredPlan = parse(&value)?;
            claimed.extend(plan.public.items.into_iter().map(|i| i.artifact_id));
        }
        let caller = system_caller();
        for (_, id) in due.into_iter().take(100) {
            if claimed.contains(&id) {
                continue;
            }
            let plan = self.plan(
                api::PlanRequest {
                    artifact_ids: vec![id],
                    automatic: true,
                    include_persistent_data: true,
                },
                &caller,
            )?;
            if plan.ready && !plan.items.iter().any(|i| claimed.contains(&i.artifact_id)) {
                claimed.extend(plan.items.iter().map(|i| i.artifact_id.clone()));
                self.start(
                    api::Start {
                        idempotency_key: format!("automatic-{}", plan.plan_id),
                        plan_id: plan.plan_id,
                    },
                    &caller,
                )?;
            }
        }
        Ok(())
    }

    fn next_eligibility_deadline(&self) -> Result<Option<u64>, ProtocolError> {
        let projection = self.projection()?;
        let mut deadline = None;
        for r in self
            .records()?
            .into_values()
            .filter(|r| r.artifact.removed_at_ms.is_none())
        {
            if !projection
                .policy(r.artifact.repository_id.as_deref())
                .automatic
            {
                continue;
            }
            let a = self.project_with(r, &projection)?;
            if let Some(at) = a.eligible_at_ms.filter(|at| *at > projection.now) {
                deadline = Some(deadline.map_or(at, |previous: u64| previous.min(at)));
            }
        }
        Ok(deadline)
    }

    fn pending_jobs(&self) -> Result<Vec<(api::Job, u32)>, ProtocolError> {
        let rows=self.database.call(|c|{let mut q=c.prepare("SELECT job_json,caller_uid FROM storage_jobs WHERE state IN ('queued','cancelling') ORDER BY created_at_ms")?;Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u32>(1)?)))?.collect::<Result<Vec<_>,_>>()?)}).map_err(db_error)?;
        rows.into_iter()
            .map(|(r, uid)| parse(&r).map(|r| (r, uid)))
            .collect()
    }

    fn recover_jobs(&self) -> Result<(), ProtocolError> {
        self.database.call(|c| {
            c.execute("DELETE FROM storage_resource_locks WHERE job_id IN (SELECT job_id FROM storage_jobs WHERE state NOT IN ('running','queued','cancelling'))",[])?;
            Ok(())
        }).map_err(db_error)?;
        let rows = self
            .database
            .call(|c| {
                let mut q = c.prepare("SELECT job_json FROM storage_jobs WHERE state='running'")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(db_error)?;
        for value in rows {
            let mut job: api::Job = parse(&value)?;
            job.state = api::JobState::Queued;
            self.save_job(&job)?;
        }
        Ok(())
    }

    pub(super) fn finish(
        &self,
        id: &str,
        state: api::JobState,
        error: Option<String>,
    ) -> Result<(), ProtocolError> {
        let id = id.to_owned();
        let now = self.now_ms();
        let job = self
            .database
            .transaction(move |c| {
                let value = c.query_row(
                    "SELECT job_json FROM storage_jobs WHERE job_id=?1",
                    [&id],
                    |r| r.get::<_, String>(0),
                )?;
                let mut job: api::Job = parse(&value).map_err(DatabaseError::Domain)?;
                let cancelled = matches!(state, api::JobState::Cancelled)
                    || matches!(
                        job.state,
                        api::JobState::Cancelling | api::JobState::Cancelled
                    );
                job.state = if cancelled {
                    api::JobState::Cancelled
                } else {
                    state
                };
                job.error_code = if cancelled { None } else { error };
                job.completed_at_ms.get_or_insert(now);
                if cancelled && let Some(plan_id) = &job.plan_id {
                    let value = c.query_row(
                        "SELECT plan_json FROM storage_plans WHERE plan_id=?1",
                        [plan_id],
                        |r| r.get::<_, String>(0),
                    )?;
                    let plan: super::model::StoredPlan =
                        parse(&value).map_err(DatabaseError::Domain)?;
                    for item in plan.public.items {
                        if job
                            .receipts
                            .iter()
                            .any(|r| r.artifact_id == item.artifact_id)
                        {
                            continue;
                        }
                        let receipt = api::ItemReceipt {
                            artifact_id: item.artifact_id,
                            status: "cancelled".into(),
                            code: Some("cleanup_cancelled".into()),
                            measured_bytes_before: item.allocated_bytes,
                            removed_at_ms: None,
                            space_change: None,
                        };
                        c.execute(
                            "INSERT INTO storage_item_receipts VALUES(?1,?2,?3,?4)",
                            rusqlite::params![
                                id,
                                receipt.artifact_id,
                                job.receipts.len() as i64,
                                json(&receipt).map_err(DatabaseError::Domain)?
                            ],
                        )?;
                        job.receipts.push(receipt);
                    }
                }
                c.execute(
                    "UPDATE storage_jobs SET state=?1,job_json=?2 WHERE job_id=?3",
                    rusqlite::params![
                        atom(&job.state),
                        json(&job).map_err(DatabaseError::Domain)?,
                        id
                    ],
                )?;
                Ok(job)
            })
            .map_err(db_error)?;
        self.publish(
            &job,
            if matches!(job.state, api::JobState::Failed | api::JobState::Partial) {
                "failed"
            } else {
                "finished"
            },
        );
        if matches!(job.state, api::JobState::Completed)
            && (job.kind == "legacy_registration"
                || (job.kind == "cleanup" && job.receipts.iter().any(|r| r.status == "removed")))
        {
            // Completed removal can release another resource's final consumer.
            // Re-observe those dependencies without retrying a cancelled or
            // failed operation in a discovery loop.
            self.request_discovery();
        }
        Ok(())
    }

    fn publish(&self, job: &api::Job, event: &str) {
        let now = self
            .clock
            .now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let _ = self.events.publish(NewEvent {
            occurred_at: now,
            dedupe_key: Some(format!(
                "storage:{}:{}:{}",
                job.job_id,
                event,
                job.receipts.len()
            )),
            event: OwnedEvent::Other(OtherOwnedEvent {
                review: None,
                kind: format!("storage.job.{event}"),
                repository_id: None,
                deployment_id: None,
                subject_kind: "storage_job".into(),
                subject_id: job.job_id.clone(),
            }),
        });
    }
}

fn collect_declared(v: &serde_json::Value, root: &std::path::Path, context: &mut Context) {
    if let Some(o) = v.as_object() {
        for (k, v) in o {
            match k.as_str() {
                "persistent_paths" => {
                    if let Some(paths) = v.as_array() {
                        for p in paths.iter().filter_map(|p| p.as_str()) {
                            context.current_paths.push(root.join(p));
                        }
                    }
                }
                "image" => {
                    if let Some(image) = v.as_str() {
                        context.current_images.push(image.into());
                    }
                }
                _ => collect_declared(v, root, context),
            }
        }
    } else if let Some(a) = v.as_array() {
        for v in a {
            collect_declared(v, root, context);
        }
    }
}
fn system_caller() -> Caller {
    Caller {
        via_edge: false,
        pid: std::process::id(),
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        client_kind: ClientKind::Other,
        client_session: Some("storage-maintenance".into()),
        work: None,
        identity: None,
    }
}
