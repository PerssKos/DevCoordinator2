use super::{
    StorageService, blocked, conflict, db_error, hash, json,
    model::{Context, Discovery, Repository, StoredJobRequest, atom},
    parse, unavailable,
};
use crate::database::OptionalExtension;
use crate::{access::Caller, database::DatabaseError, events::NewEvent};
use devcoordinator2_api::{
    ClientKind, ProtocolError,
    results::{OtherOwnedEvent, OwnedEvent},
    storage as api,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;

/// Private discovery state is retained by the active job, never an API reply.
pub struct ScanCursor {
    job_id: String,
    context: Context,
    sequence: u64,
    discovery: super::native::DiscoveryCursor,
    reconciliation: Option<super::reconcile::ReconcileCursor>,
    last_rows: u64,
    last_bytes: u64,
    failure: Option<ProtocolError>,
    #[cfg(feature = "root-acceptance")]
    request_key: String,
}

struct ScanJob {
    service: StorageService,
    id: String,
    cursor: std::sync::Mutex<Option<ScanCursor>>,
    terminal_error: std::sync::Mutex<Option<ProtocolError>>,
}

#[cfg(test)]
pub(super) fn scan_job_for_test(
    service: StorageService,
    id: String,
    cursor: Option<ScanCursor>,
) -> std::sync::Arc<dyn crate::maintenance::Job> {
    assert!(service.begin_job(&id).unwrap().is_some());
    std::sync::Arc::new(ScanJob {
        service,
        id,
        cursor: std::sync::Mutex::new(cursor),
        terminal_error: std::sync::Mutex::new(None),
    })
}

impl crate::maintenance::Job for ScanJob {
    fn name(&self) -> &'static str {
        "storage_discovery"
    }
    fn key(&self) -> String {
        format!("storage:{}", self.id)
    }
    fn run(
        &self,
        budget: &mut crate::maintenance::Budget,
        _cursor: Option<String>,
    ) -> crate::maintenance::JobResult {
        use crate::maintenance::{JobResult, JobStatus};
        if budget.exhausted() {
            return JobResult {
                status: JobStatus::Continue {
                    cursor: self.id.clone(),
                },
                rows: 0,
                bytes: 0,
            };
        }
        let mut cursor = self
            .cursor
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let result = self.service.execute_scan_step(
            &self.id,
            &mut cursor,
            super::native::DiscoveryBudget {
                entries: 256,
                elapsed: budget.remaining().min(Duration::from_millis(25)),
            },
        );
        if let Some(state) = cursor.as_ref() {
            budget.consume(state.last_rows, state.last_bytes);
        }
        let status = match result {
            Ok(true) => JobStatus::Complete,
            Ok(false) => JobStatus::Continue {
                cursor: self.id.clone(),
            },
            Err(error) => {
                let reason = error.message.clone();
                // Busy ownership contention and a temporary authority-database
                // outage keep the resumable cursor alive. Other errors have
                // already passed through reconciliation and are terminal for
                // this accepted scan.
                if !Self::is_transient_error(&error) {
                    *self
                        .terminal_error
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = Some(error);
                    return JobResult {
                        status: JobStatus::Failed { reason },
                        rows: budget.rows(),
                        bytes: budget.bytes(),
                    };
                }
                JobStatus::Retry { reason }
            }
        };
        JobResult {
            status,
            rows: budget.rows(),
            bytes: budget.bytes(),
        }
    }
}

impl ScanJob {
    fn is_transient_error(error: &ProtocolError) -> bool {
        error.code == devcoordinator2_api::ErrorCode::Busy
            || (error.code == devcoordinator2_api::ErrorCode::StorageUnavailable
                && error.message == "storage_database_unavailable")
    }

    fn take_terminal_error(&self) -> Option<ProtocolError> {
        self.terminal_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }
}

#[cfg(feature = "root-acceptance")]
const ROOT_ACCEPTANCE_SCAN_BARRIER_TIMEOUT: Duration = Duration::from_secs(300);

fn scan_after_request(now: u64, last_started: Option<u64>) -> u64 {
    last_started
        .map(|at| at.saturating_add(super::STORAGE_SCAN_MIN_GAP_MS))
        .unwrap_or(now.saturating_add(1000))
        .max(now)
}

fn scan_after_completion(now: u64) -> u64 {
    now.saturating_add(super::STORAGE_SCAN_MIN_GAP_MS)
}

impl StorageService {
    pub fn execute_scan_step(
        &self,
        id: &str,
        cursor: &mut Option<ScanCursor>,
        budget: super::native::DiscoveryBudget,
    ) -> Result<bool, ProtocolError> {
        if cursor.as_ref().is_some_and(|state| state.job_id != id) {
            return Err(blocked("storage_scan_cursor_mismatch"));
        }
        let job = self.job(id)?;
        if job.kind != "scan" {
            return Err(blocked("storage_job_is_not_scan"));
        }
        if matches!(
            job.state,
            api::JobState::Cancelling | api::JobState::Cancelled
        ) {
            if matches!(job.state, api::JobState::Cancelling) {
                self.finish(id, api::JobState::Cancelled, None)?;
            }
            *cursor = None;
            return Ok(true);
        }
        if cursor.is_none() {
            let (raw, caller_uid) = self
                .database
                .read(|connection| {
                    Ok(connection.query_row(
                        "SELECT request_json,caller_uid FROM storage_jobs WHERE job_id=$1",
                        [id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                    )?)
                })
                .map_err(db_error)?;
            let stored: serde_json::Value = parse(&raw)?;
            let (parameters, local_manual) = if stored.get("storage_request_version").is_some() {
                let StoredJobRequest::V1 {
                    parameters,
                    local_manual,
                } = parse(&raw)?;
                (parameters, local_manual)
            } else {
                (stored, false)
            };
            let request: api::Scan = serde_json::from_value(parameters)
                .map_err(|_| blocked("storage_scan_request_invalid"))?;
            let manual_caller_uid = local_manual
                .then(|| self.local_execution_uid(caller_uid))
                .flatten();
            let mut context = self.context_for_uid(manual_caller_uid)?;
            context.scan_repository_id = request.repository_id;
            let sequence = self
                .database
                .maintenance_transaction(super::next_sequence)
                .map_err(db_error)?;
            *cursor = Some(ScanCursor {
                job_id: id.into(),
                context,
                sequence,
                discovery: super::native::DiscoveryCursor::default(),
                reconciliation: None,
                last_rows: 0,
                last_bytes: 0,
                failure: None,
                #[cfg(feature = "root-acceptance")]
                request_key: request.idempotency_key,
            });
            return Ok(false);
        }
        let state = cursor.as_mut().expect("scan cursor initialized");
        state.last_rows = 0;
        state.last_bytes = 0;
        if let Some(reconciliation) = state.reconciliation.as_mut() {
            let (complete, rows, bytes) = reconciliation.step(self, budget.elapsed)?;
            state.last_rows = rows;
            state.last_bytes = bytes;
            if !complete {
                return Ok(false);
            }
            if let Some(error) = state.failure.take() {
                *cursor = None;
                return Err(error);
            }
            self.finish(id, api::JobState::Completed, None)?;
            self.queue_due_cleanup()?;
            *cursor = None;
            return Ok(true);
        }
        let result = self
            .backend
            .discover_step(&state.context, &mut state.discovery, budget);
        let result = match result {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(false),
            // Retry transient ownership contention with the current scan
            // intact. Reconciliation below is only for permanent failures.
            Err(error) if error.code == devcoordinator2_api::ErrorCode::Busy => return Err(error),
            Err(error) => {
                state.reconciliation = Some(super::reconcile::ReconcileCursor::new(
                    self,
                    Discovery {
                        repository_id: state.context.scan_repository_id.clone(),
                        coverage_gaps: vec!["discovery_failed".into()],
                        ..Discovery::default()
                    },
                    state.sequence,
                    state.context.manual_caller_uid,
                )?);
                state.failure = Some(error);
                return Ok(false);
            }
        };
        #[cfg(feature = "root-acceptance")]
        if self
            .config
            .unit_prefix
            .starts_with("devcoordinator2-rustint-")
        {
            let barrier = self.config.state_dir.join("storage-pause-scan");
            if std::fs::read_to_string(&barrier).ok().as_deref() == Some(state.request_key.as_str())
            {
                std::fs::write(barrier.with_extension("ready"), id)
                    .map_err(|_| blocked("fixture_scan_barrier_unavailable"))?;
                let deadline = std::time::Instant::now() + ROOT_ACCEPTANCE_SCAN_BARRIER_TIMEOUT;
                while barrier.exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(blocked("fixture_scan_barrier_timeout"));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
        state.reconciliation = Some(super::reconcile::ReconcileCursor::new(
            self,
            result,
            state.sequence,
            state.context.manual_caller_uid,
        )?);
        Ok(false)
    }
    fn has_intent(&self, job: &str, record: &super::model::Record) -> Result<bool, ProtocolError> {
        let job = job.to_owned();
        let record = record.clone();
        self.database.read(move |c| {
            let intent = c.query_row("SELECT fingerprint,revision FROM storage_item_intents WHERE job_id=$1 AND artifact_id=$2",
                crate::database::params![job,record.artifact.artifact_id], |r| Ok((r.get::<_,String>(0)?,super::super_sql_u64(r.get::<_,i64>(1)?)?))).optional()?;
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
                    "SELECT state FROM storage_jobs WHERE job_id=$1 FOR UPDATE",
                    [&job],
                    |r| r.get(0),
                )?;
                if state != "running" {
                    return Err(DatabaseError::Domain(blocked("cleanup_cancelled")));
                }
                let revision: i64 = c.query_row(
                    "SELECT revision FROM storage_artifacts WHERE artifact_id=$1 FOR UPDATE",
                    [&record.artifact.artifact_id],
                    |r| r.get(0),
                )?;
                if revision != record.artifact.revision as i64 {
                    return Err(DatabaseError::Domain(conflict("artifact_changed")));
                }
                c.execute(
                    "INSERT INTO storage_item_intents VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
                    crate::database::params![
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
            let value=c.query_row("SELECT job_json FROM storage_jobs WHERE job_id=$1",[&job_id],|r|r.get::<_,String>(0))?;
            let mut job:api::Job=parse(&value).map_err(DatabaseError::Domain)?;
            let step=job.receipts.len() as u32;
            let changed=c.execute("UPDATE storage_artifacts SET removed_at_ms=$1,revision=$2,record_json=$3,updated_at_ms=$4 WHERE artifact_id=$5 AND revision=$6",crate::database::params![record.artifact.removed_at_ms.map(|n|n as i64),record.artifact.revision as i64,json(&record).map_err(DatabaseError::Domain)?,now as i64,record.artifact.artifact_id,(record.artifact.revision-1) as i64])?;
            if changed != 1 { return Err(DatabaseError::Domain(conflict("artifact_changed"))); }
            c.execute("INSERT INTO storage_item_receipts VALUES($1,$2,$3,$4)",crate::database::params![job_id,receipt.artifact_id,step,json(&receipt).map_err(DatabaseError::Domain)?])?;
            job.receipts.push(receipt);job.reclaimed_bytes=job.reclaimed_bytes.saturating_add(added);
            if job.receipts.last().is_some_and(|r| matches!(r.status.as_str(),"removed"|"partial") && r.space_change.is_none() && r.code.as_deref()!=Some("removed_shared_reference")) && record.artifact.kind != api::Kind::Mount {
                job.unmeasured_items += 1;
            }
            // Preserve a cancellation committed while the native step was running.
            c.execute("UPDATE storage_jobs SET job_json=$1 WHERE job_id=$2",crate::database::params![json(&job).map_err(DatabaseError::Domain)?,job_id])?;
            Ok(job)
        }).map_err(db_error)
    }

    fn begin_job(&self, id: &str) -> Result<Option<api::Job>, ProtocolError> {
        let id = id.to_owned();
        let now = self.now_ms();
        self.database
            .transaction(move |c| {
                let value = c.query_row(
                    "SELECT job_json FROM storage_jobs WHERE job_id=$1",
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
                        "UPDATE storage_jobs SET state='cancelled',job_json=$1 WHERE job_id=$2",
                        crate::database::params![json(&job).map_err(DatabaseError::Domain)?, id],
                    )?;
                    return Ok(None);
                }
                if !matches!(job.state, api::JobState::Queued) {
                    return Ok(None);
                }
                job.state = api::JobState::Running;
                job.started_at_ms.get_or_insert(now);
                c.execute(
                    "UPDATE storage_jobs SET state='running',job_json=$1 WHERE job_id=$2",
                    crate::database::params![json(&job).map_err(DatabaseError::Domain)?, id],
                )?;
                Ok(Some(job))
            })
            .map_err(db_error)
    }

    #[cfg(test)]
    pub(crate) fn context(&self) -> Result<Context, ProtocolError> {
        self.context_for_uid(None)
    }

    pub(crate) fn manual_caller_uid(&self, caller: &Caller) -> Option<u32> {
        (!caller.is_console())
            .then(|| self.local_execution_uid(caller.uid))
            .flatten()
    }

    fn local_execution_uid(&self, uid: u32) -> Option<u32> {
        (uid != 0 && self.config.edge_uid != Some(uid)).then_some(uid)
    }

    pub(crate) fn context_for_uid(
        &self,
        manual_caller_uid: Option<u32>,
    ) -> Result<Context, ProtocolError> {
        let now = self.now_ms();
        let (repositories,worktrees,deployments,generations,source_origins,components,observed,containers,leases)=self.database.read(move|c|{
            let repositories={let mut q=c.prepare("SELECT repository_id,display_name,root_path,registered_by_uid FROM repositories ORDER BY repository_id")?;q.query_map([],|r|Ok(Repository {id:r.get(0)?,name:r.get(1)?,root:PathBuf::from(r.get::<_,String>(2)?),execution_uid:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?};
            let worktrees={let mut q=c.prepare("SELECT repository_id,worktree_path FROM worktrees ORDER BY worktree_id")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,PathBuf::from(r.get::<_,String>(1)?))))?.collect::<Result<Vec<_>,_>>()?};
            let deployments={let mut q=c.prepare("SELECT d.deployment_id,d.repository_id,d.name,d.source,d.spec_json,w.worktree_path FROM deployments d JOIN worktrees w ON w.worktree_id=d.worktree_id")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,PathBuf::from(r.get::<_,String>(5)?))))?.collect::<Result<Vec<_>,_>>()?};
            let generations={let mut q=c.prepare("SELECT g.deployment_id,g.path FROM generations g JOIN deployments d ON d.deployment_id=g.deployment_id WHERE g.number=d.current_generation OR g.number=d.previous_generation")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,PathBuf::from(r.get::<_,String>(1)?))))?.collect::<Result<Vec<_>,_>>()?};
            let source_origins={let mut q=c.prepare("SELECT o.path FROM generation_source_origins o JOIN deployments d ON d.deployment_id=o.deployment_id WHERE o.number=d.current_generation OR o.number=d.previous_generation")?;q.query_map([],|r|Ok(PathBuf::from(r.get::<_,String>(0)?)))?.collect::<Result<Vec<_>,_>>()?};
            let components={let mut q=c.prepare("SELECT deployment_id,binding_kind,binding_identity FROM components WHERE binding_identity IS NOT NULL")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?)))?.collect::<Result<Vec<_>,_>>()?};
            let observed={let mut q=c.prepare("SELECT observed_deployment_id,repository_id,native_project FROM observed_deployments")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,(r.get::<_,String>(1)?,r.get::<_,String>(2)?))))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let containers={let mut q=c.prepare("SELECT container_id,repository_id,observed_deployment_id FROM observed_containers")?;q.query_map([],|r|Ok((r.get::<_,String>(0)?,(r.get::<_,String>(1)?,r.get::<_,String>(2)?))))?.collect::<Result<BTreeMap<_,_>,_>>()?};
            let leases={let mut q=c.prepare("SELECT lease_json FROM storage_leases WHERE expires_at_ms>$1")?;q.query_map([now as i64],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?};
            Ok((repositories,worktrees,deployments,generations,source_origins,components,observed,containers,leases))
        }).map_err(db_error)?;
        let mut context = Context {
            manual_caller_uid,
            repositories,
            worktrees,
            roots: self.root_records()?,
            observed,
            recorded_containers: containers,
            now_ms: now,
            ..Context::default()
        };
        let reviewed = self.database.read(|connection| {
            let mut query = connection.prepare(
                "SELECT record_json FROM storage_artifacts WHERE kind='worktree' AND removed_at_ms IS NULL AND jsonb_typeof(record_json->'ignored_review')='object' ORDER BY artifact_id LIMIT 1025",
            )?;
            Ok(query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        }).map_err(db_error)?;
        if reviewed.len() > 1024 {
            return Err(unavailable("ignored_review_catalogue_too_large"));
        }
        for value in reviewed {
            let record: super::model::Record = parse(&value)?;
            if let Some(review) = record.ignored_review {
                context.ignored_reviews.insert(record.resource_key, review);
            }
        }
        // Git also knows linked worktrees that have never run a Coordinator
        // command. Inspect it under this operation's non-root execution identity.
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
        let protected = self.database.read(|connection| {
            let mut query=connection.prepare("SELECT projection_resource_key,projection_ancestors FROM storage_artifacts WHERE removed_at_ms IS NULL AND projection_protected")?;
            Ok(query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(db_error)?;
        for (key, ancestors) in protected {
            context.protected_resources.push(key);
            context
                .protected_ancestors
                .extend(parse::<Vec<String>>(&ancestors)?);
        }
        let (age,depth)=self.database.read(|c|c.query_row("SELECT max_age_seconds,case_depth FROM test_log_retention_state WHERE singleton=1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).map_err(DatabaseError::from)).map_err(db_error)?;
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
        // Candidate origins are selected-generation references even when the
        // declared checkout has since removed its deployment declaration.
        context.current_paths.extend(source_origins);
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
        let ids = json(&context.leased_artifacts)?;
        let leased = self.database.read(|connection| {
            let mut query=connection.prepare("SELECT projection_resource_key,projection_ancestors FROM storage_artifacts WHERE artifact_id IN (SELECT value FROM jsonb_array_elements_text($1::jsonb))")?;
            Ok(query.query_map([ids],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(db_error)?;
        for (key, ancestors) in leased {
            context.leased_resources.push(key);
            context
                .leased_ancestors
                .extend(parse::<Vec<String>>(&ancestors)?);
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

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(Self) -> Result<T, ProtocolError> + Send + 'static,
    ) -> Result<T, ProtocolError> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || crate::database::maintenance_context(|| work(service)))
            .await
            .map_err(|_| blocked("storage_worker_interrupted"))?
    }

    pub async fn serve(&self, mut shutdown: watch::Receiver<bool>) {
        let _ = self.blocking(|service| service.recover_jobs()).await;
        let mut workers = tokio::task::JoinSet::new();
        let mut active = BTreeSet::new();
        // A host-wide scan can legitimately walk millions of entries. Do not
        // start one during daemon initialization, and never immediately chain
        // another global scan after a slow run or an event burst.
        let mut next_scan = self.now_ms().saturating_add(super::STORAGE_SCAN_MIN_GAP_MS);
        let mut last_scan_started: Option<u64> = None;
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
                let now = self.now_ms();
                next_scan = next_scan.min(scan_after_request(now, last_scan_started));
            }
            if self.now_ms() >= next_scan {
                let now = self.now_ms();
                last_scan_started = Some(now);
                let caller = system_caller();
                let scanning = self.blocking(|service| service.database.read(|c| c.query_row("SELECT EXISTS(SELECT 1 FROM storage_jobs WHERE kind='scan' AND state IN ('queued','running') AND COALESCE(request_json::jsonb->'parameters'->>'repository_id',request_json::jsonb->>'repository_id') IS NULL)",[],|r|r.get::<_,bool>(0)).map_err(DatabaseError::from)).map_err(db_error)).await.unwrap_or(true);
                if scanning {
                    rescan_requested = true;
                } else {
                    let _ = self
                        .blocking(move |service| {
                            service.scan(
                                api::Scan {
                                    repository_id: None,
                                    idempotency_key: format!(
                                        "scheduled-{now}-{}",
                                        event_cursor.unwrap_or(0)
                                    ),
                                },
                                &caller,
                            )
                        })
                        .await;
                }
                next_scan = now.saturating_add(super::STORAGE_SCAN_INTERVAL_MS);
            }
            // The observation window and lease end are deadlines, not merely
            // hints for the next hourly scan. Deletion still follows a fresh scan.
            if let Ok(deadline) = self
                .blocking(|service| service.next_eligibility_deadline())
                .await
                && let Some(deadline) = deadline
            {
                next_scan = next_scan.min(deadline);
            }
            if let Ok(jobs) = self.blocking(|service| service.pending_jobs()).await {
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
            let subscription = self
                .blocking(move |service| {
                    service.events.subscribe(
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
                    )
                })
                .await;
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
                event=lifecycle=>{if let Some(event)=event {event_cursor=Some(event.cursor);if !event.events.is_empty(){let now=self.now_ms();next_scan=next_scan.min(scan_after_request(now,last_scan_started));}}},
                result=workers.join_next(),if !workers.is_empty()=>{if let Some(Ok((id,())))=result{active.remove(&id);if rescan_requested {next_scan=scan_after_completion(self.now_ms());rescan_requested=false;}}},
                changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break;}},
            }
        }
        while workers.join_next().await.is_some() {}
    }

    async fn run_job(&self, mut job: api::Job, uid: u32, mut shutdown: watch::Receiver<bool>) {
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
                let run = run.clone();
                let _ = self
                    .blocking(move |service| service.capacity.unregister_run(&run))
                    .await;
                return;
            }
            let mut permit = Some(permit);
            let begin_id = job.job_id.clone();
            if let Ok(Some(current)) = self
                .blocking(move |service| {
                    let current = service.begin_job(&begin_id)?;
                    if let Some(current) = &current {
                        service.publish(current, "started");
                    }
                    Ok(current)
                })
                .await
            {
                job = current;
                let service = self.clone();
                let id = job.job_id.clone();
                let result = if job.kind == "scan" {
                    let scheduler = self
                        .maintenance_scheduler
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .upgrade();
                    match scheduler {
                        None => Ok(Err(blocked("maintenance_scheduler_unavailable"))),
                        Some(scheduler) => {
                            let scan = std::sync::Arc::new(ScanJob {
                                service,
                                id,
                                cursor: std::sync::Mutex::new(None),
                                terminal_error: std::sync::Mutex::new(None),
                            });
                            let mut continuation = None;
                            let mut retry_events = scheduler.retry_events();
                            loop {
                                if *shutdown.borrow() {
                                    break Ok(Err(blocked("storage_scan_interrupted")));
                                }
                                let step_permit = match permit.take() {
                                    Some(permit) => permit,
                                    None => match self
                                        .capacity
                                        .admit_native(&run, "storage", uid, shutdown.clone())
                                        .await
                                    {
                                        Ok(permit) => permit,
                                        Err(error) => break Ok(Err(error)),
                                    },
                                };
                                retry_events.borrow_and_update();
                                let reply = scheduler
                                    .submit_async(
                                        scan.clone(),
                                        crate::maintenance::Budget::new(
                                            Duration::from_millis(25),
                                            256,
                                            256 * 1024,
                                        ),
                                        continuation.clone(),
                                    )
                                    .await;
                                // A resumable scan owns capacity only for this
                                // batch, including when maintenance defers it.
                                // Existing queued cleanup and tests get a turn
                                // before the next discovery batch is admitted.
                                drop(step_permit);
                                let retry_at = match reply {
                                    Ok(reply) => match reply.status {
                                        crate::maintenance::JobStatus::Complete => {
                                            break Ok(Ok(()));
                                        }
                                        crate::maintenance::JobStatus::Deferred {
                                            not_before_ms,
                                        } => Some(not_before_ms),
                                        crate::maintenance::JobStatus::Continue { cursor } => {
                                            continuation = Some(cursor);
                                            continue;
                                        }
                                        crate::maintenance::JobStatus::Retry { .. } => None,
                                        crate::maintenance::JobStatus::Failed { reason } => {
                                            if let Some(error) = scan.take_terminal_error() {
                                                break Ok(Err(error));
                                            }
                                            break Ok(Err(ProtocolError::new(
                                                devcoordinator2_api::ErrorCode::StorageUnavailable,
                                                reason,
                                            )));
                                        }
                                    },
                                    Err(_) => {
                                        // A full queue, a database outage or another live
                                        // lease is not a completed scan. Keep the accepted
                                        // durable job and retry while the service is active.
                                        None
                                    }
                                };
                                if !scheduler
                                    .wait_retry(&mut retry_events, retry_at, &mut shutdown)
                                    .await
                                {
                                    break Ok(Err(blocked("storage_scan_interrupted")));
                                }
                            }
                        }
                    }
                } else {
                    tokio::task::spawn_blocking(move || {
                        crate::database::maintenance_context(|| service.execute_job(&id))
                    })
                    .await
                    .map_err(|error| {
                        ProtocolError::new(
                            devcoordinator2_api::ErrorCode::InternalError,
                            "storage worker interrupted",
                        )
                        .with_detail(error.to_string())
                    })
                };
                if !(matches!(result, Ok(Ok(()))) || job.kind == "scan" && *shutdown.borrow()) {
                    let failed_job = job.clone();
                    let _ = self
                        .blocking(move |service| {
                            let mut latest = service.job(&failed_job.job_id).unwrap_or(failed_job);
                            latest.state = if latest.receipts.iter().any(|r| r.status == "removed")
                            {
                                api::JobState::Partial
                            } else {
                                api::JobState::Failed
                            };
                            latest.error_code = Some(match result {
                                Ok(Err(e)) => e.message,
                                _ => "storage_worker_interrupted".into(),
                            });
                            service.finish(&latest.job_id, latest.state, latest.error_code)
                        })
                        .await;
                }
            }
            drop(permit);
        }
        let run = run.clone();
        let _ = self
            .blocking(move |service| service.capacity.unregister_run(&run))
            .await;
        self.wake.notify_waiters();
    }

    /// Finite execution boundary used by the native worker and acceptance fixtures.
    pub fn execute_job(&self, id: &str) -> Result<(), ProtocolError> {
        let job = self.job(id)?;
        let key = id.to_owned();
        let (raw, caller_uid) = self
            .database
            .read(move |c| {
                c.query_row(
                    "SELECT request_json,caller_uid FROM storage_jobs WHERE job_id=$1",
                    [key],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
                )
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        let stored: serde_json::Value = parse(&raw)?;
        let (raw, local_manual) = if stored.get("storage_request_version").is_some() {
            let StoredJobRequest::V1 {
                parameters,
                local_manual,
            } = parse(&raw)?;
            (json(&parameters)?, local_manual)
        } else {
            // Legacy jobs cannot prove whether their anonymous peer was the
            // Console edge before a config change. Preserve their old behavior.
            (raw, false)
        };
        // The kernel UID and original transport classification are both durable;
        // neither request fields nor a later edge UID change can replace them.
        let manual_caller_uid = local_manual
            .then(|| self.local_execution_uid(caller_uid))
            .flatten();
        match job.kind.as_str() {
            "scan" => {
                let mut cursor = None;
                while !self.execute_scan_step(
                    id,
                    &mut cursor,
                    super::native::DiscoveryBudget::default(),
                )? {}
            }
            "legacy_registration" => {
                let request: api::LegacyRegister = parse(&raw)?;
                let context = self.context_for_uid(manual_caller_uid)?;
                let records = self.records()?.into_values().collect::<Vec<_>>();
                let ids = self
                    .backend
                    .legacy_group(&request.deployment_id, &records, &context)?;
                let mut owned = ids
                    .iter()
                    .map(|id| self.record(id))
                    .collect::<Result<Vec<_>, _>>()?;
                let _resources = self.lock_resources(&job.job_id, &owned)?;
                let context = self.context_for_uid(manual_caller_uid)?;
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
                        let changed = c.execute("UPDATE storage_artifacts SET revision=$1,record_json=$2,updated_at_ms=$3 WHERE artifact_id=$4 AND revision=$5",crate::database::params![r.artifact.revision as i64,json(&r).map_err(DatabaseError::Domain)?,now as i64,id,(r.artifact.revision-1) as i64])?;
                        if changed != 1 { return Err(DatabaseError::Domain(conflict("artifact_changed"))); }
                        super::change(c,Some(id),"legacy_disposal",&actor,now,"{}")?;
                    }
                    Ok(())
                }).map_err(db_error)?;
                self.finish(id, api::JobState::Completed, None)?;
            }
            "cleanup" => {
                let request: api::Start = parse(&raw)?;
                if job.plan_id.as_deref() != Some(request.plan_id.as_str()) {
                    return Err(blocked("stored_storage_record_invalid"));
                }
                self.execute_cleanup(&job, manual_caller_uid)?;
            }
            _ => return Err(blocked("unknown_storage_job")),
        }
        Ok(())
    }

    fn execute_cleanup(
        &self,
        job: &api::Job,
        manual_caller_uid: Option<u32>,
    ) -> Result<(), ProtocolError> {
        let plan = self.stored_plan(
            job.plan_id
                .as_deref()
                .ok_or_else(|| blocked("cleanup_plan_missing"))?,
        )?;
        #[cfg(feature = "root-acceptance")]
        if self
            .config
            .unit_prefix
            .starts_with("devcoordinator2-rustint-")
        {
            let barrier = self.config.state_dir.join("storage-pause-before-cleanup");
            if std::fs::read_to_string(&barrier).ok().as_deref()
                == Some(plan.public.plan_id.as_str())
            {
                std::fs::write(barrier.with_extension("ready"), &job.job_id)
                    .map_err(|_| blocked("fixture_cleanup_barrier_unavailable"))?;
                let deadline = std::time::Instant::now() + Duration::from_secs(90);
                while barrier.exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(blocked("fixture_cleanup_barrier_timeout"));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
        let selected = plan
            .records
            .iter()
            .map(|r| r.artifact.artifact_id.clone())
            .collect::<Vec<_>>();
        let manual_caller_uid = manual_caller_uid.filter(|_| !plan.public.automatic);
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
                let context = self.context_for_uid(manual_caller_uid)?;
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
                        .base_artifacts(&BTreeMap::from([(
                            record.artifact.artifact_id.clone(),
                            record.clone(),
                        )]))?
                        .0
                        .get(&record.artifact.artifact_id)
                        .ok_or_else(|| conflict("artifact_changed"))?
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

    fn queue_due_cleanup(&self) -> Result<(), ProtocolError> {
        let now = self.now_ms();
        let sql = format!(
            "{} SELECT e.artifact_id FROM eligibility e WHERE e.removed_at_ms IS NULL AND e.base_safety='safe' AND e.automatic AND e.eligible_at_ms<=$1::bigint::numeric AND NOT EXISTS(SELECT 1 FROM storage_jobs j JOIN storage_plans p ON p.plan_id=j.job_json->>'plan_id' WHERE j.state IN ('queued','running','cancelling') AND p.plan_json @> jsonb_build_object('public',jsonb_build_object('items',jsonb_build_array(jsonb_build_object('artifact_id',e.artifact_id))))) ORDER BY CASE e.kind WHEN 'backing_directory' THEN 0 WHEN 'volume' THEN 1 ELSE 2 END,e.artifact_id LIMIT 100",
            super::storage_eligibility_projection()
        );
        let due = self
            .database
            .maintenance(|connection| {
                let mut query = connection.prepare(&sql)?;
                Ok(query
                    .query_map([now], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(db_error)?;
        let caller = system_caller();
        for id in due {
            // A previously queued companion may have claimed this artifact
            // since the bounded candidate query.
            if !self.cleanup_claimed(std::slice::from_ref(&id))?.is_empty() {
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
            let ids = plan
                .items
                .iter()
                .map(|item| item.artifact_id.clone())
                .collect::<Vec<_>>();
            if plan.ready && self.cleanup_claimed(&ids)?.is_empty() {
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

    fn cleanup_claimed(&self, ids: &[String]) -> Result<BTreeSet<String>, ProtocolError> {
        let encoded = json(ids)?;
        self.database.maintenance(|connection| {
            let mut query=connection.prepare("SELECT candidate.id FROM jsonb_array_elements_text($1::jsonb) candidate(id) WHERE EXISTS(SELECT 1 FROM storage_jobs j JOIN storage_plans p ON p.plan_id=j.job_json->>'plan_id' WHERE j.state IN ('queued','running','cancelling') AND p.plan_json @> jsonb_build_object('public',jsonb_build_object('items',jsonb_build_array(jsonb_build_object('artifact_id',candidate.id)))))")?;
            Ok(query.query_map([encoded],|row|row.get::<_,String>(0))?.collect::<Result<BTreeSet<_>,_>>()?)
        }).map_err(db_error)
    }

    fn next_eligibility_deadline(&self) -> Result<Option<u64>, ProtocolError> {
        let now = self.now_ms();
        let sql = format!(
            "{} SELECT MIN(eligible_at_ms)::text FROM eligibility WHERE removed_at_ms IS NULL AND automatic AND eligible_at_ms>$1::bigint::numeric",
            super::storage_eligibility_projection()
        );
        let value = self
            .database
            .maintenance(|connection| {
                connection
                    .query_row(&sql, [now], |row| row.get::<_, Option<String>>(0))
                    .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        value
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| blocked("storage_eligibility_timestamp_invalid"))
            })
            .transpose()
    }

    fn pending_jobs(&self) -> Result<Vec<(api::Job, u32)>, ProtocolError> {
        let rows=self.database.read(|c|{let mut q=c.prepare("SELECT job_json,caller_uid FROM storage_jobs WHERE state IN ('queued','cancelling') ORDER BY created_at_ms")?;Ok(q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u32>(1)?)))?.collect::<Result<Vec<_>,_>>()?)}).map_err(db_error)?;
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
            .read(|c| {
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
                    "SELECT job_json FROM storage_jobs WHERE job_id=$1",
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
                        "SELECT plan_json FROM storage_plans WHERE plan_id=$1",
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
                            "INSERT INTO storage_item_receipts VALUES($1,$2,$3,$4)",
                            crate::database::params![
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
                    "UPDATE storage_jobs SET state=$1,job_json=$2 WHERE job_id=$3",
                    crate::database::params![
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
        model: None,
        effort: None,
        client_session: Some("storage-maintenance".into()),
        work: None,
        identity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation_test_support::{Fixture, FixtureClock};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use tokio::sync::oneshot;

    #[derive(Default)]
    struct ScanDocker {
        calls: AtomicUsize,
        boundary: Mutex<Option<(oneshot::Sender<()>, mpsc::Receiver<()>)>>,
    }

    impl crate::docker::DockerControl for ScanDocker {
        fn invoke(
            &self,
            _: crate::docker::DockerInvocation,
        ) -> Result<crate::docker::DockerOutput, crate::docker::DockerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = self.boundary.lock().unwrap().take() {
                let _ = entered.send(());
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            Err(crate::docker::DockerError::CliUnavailable)
        }

        fn spawn_follow_logs(
            &self,
            _: &crate::docker::ExactContainerId,
        ) -> Result<crate::docker::LogFollower, crate::docker::DockerError> {
            panic!("scan fixture must not follow container logs")
        }
    }

    fn scan_service(fixture: &Fixture, docker: Arc<ScanDocker>) -> StorageService {
        StorageService::with_backend(
            fixture.config.clone(),
            fixture.database.clone(),
            Arc::new(FixtureClock),
            crate::capacity::CapacityBroker::new(
                fixture.database.clone(),
                fixture.config.socket_path.clone(),
            )
            .unwrap(),
            crate::events::EventService::new(fixture.database.clone()).unwrap(),
            Arc::new(super::super::native::HostBackend::with_docker(
                fixture.config.clone(),
                fixture.database.clone(),
                docker,
            )),
        )
    }

    struct FailureBackend {
        code: devcoordinator2_api::ErrorCode,
        message: &'static str,
    }

    impl super::super::native::Backend for FailureBackend {
        fn discover(&self, _context: &Context) -> Result<Discovery, ProtocolError> {
            Err(ProtocolError::new(self.code, self.message))
        }

        fn discover_step(
            &self,
            _context: &Context,
            _cursor: &mut super::super::native::DiscoveryCursor,
            _budget: super::super::native::DiscoveryBudget,
        ) -> Result<Option<Discovery>, ProtocolError> {
            Err(ProtocolError::new(self.code, self.message))
        }

        fn validate(
            &self,
            _record: &super::super::model::Record,
            _context: &Context,
            _selected: &[String],
        ) -> Result<(), ProtocolError> {
            Ok(())
        }

        fn validate_disposal_intent(
            &self,
            _record: &super::super::model::Record,
            _repository_id: &str,
            _now_ms: u64,
            _reviewed_ignored_files: Option<&[api::IgnoredFileReview]>,
        ) -> Result<(String, Option<super::super::model::IgnoredReview>), ProtocolError> {
            Ok(("fixture".into(), None))
        }

        fn remove(
            &self,
            _record: &super::super::model::Record,
            _context: &Context,
            _selected: &[String],
            _job_id: &str,
        ) -> Result<(), ProtocolError> {
            Ok(())
        }

        fn absent(
            &self,
            _record: &super::super::model::Record,
            _context: &Context,
        ) -> Result<bool, ProtocolError> {
            Ok(true)
        }

        fn save_recovery(
            &self,
            _record: &super::super::model::Record,
            _job_id: &str,
        ) -> Result<(), ProtocolError> {
            Ok(())
        }

        fn resume(
            &self,
            _record: &super::super::model::Record,
            _context: &Context,
            _selected: &[String],
            _job_id: &str,
        ) -> Result<(), ProtocolError> {
            Ok(())
        }

        fn legacy_group(
            &self,
            _deployment_id: &str,
            _records: &[super::super::model::Record],
            _context: &Context,
        ) -> Result<Vec<String>, ProtocolError> {
            Ok(Vec::new())
        }
    }

    fn failure_scan_service(
        fixture: &Fixture,
        code: devcoordinator2_api::ErrorCode,
        message: &'static str,
    ) -> StorageService {
        StorageService::with_backend(
            fixture.config.clone(),
            fixture.database.clone(),
            Arc::new(FixtureClock),
            crate::capacity::CapacityBroker::new(
                fixture.database.clone(),
                fixture.config.socket_path.clone(),
            )
            .unwrap(),
            crate::events::EventService::new(fixture.database.clone()).unwrap(),
            Arc::new(FailureBackend { code, message }),
        )
    }

    #[test]
    fn cancelled_storage_scan_stops_before_another_native_step() {
        let fixture = Fixture::new();
        let docker = Arc::new(ScanDocker::default());
        let service = scan_service(&fixture, docker.clone());
        let caller = system_caller();
        let scan = service
            .scan(
                api::Scan {
                    repository_id: Some("project-alpha".into()),
                    idempotency_key: "cancel-initialized-scan".into(),
                },
                &caller,
            )
            .unwrap();
        assert!(service.begin_job(&scan.job_id).unwrap().is_some());
        let mut cursor = None;
        let budget = super::super::native::DiscoveryBudget::default();
        assert!(
            !service
                .execute_scan_step(&scan.job_id, &mut cursor, budget)
                .unwrap()
        );
        assert!(cursor.is_some());
        service
            .cancel(
                api::JobReference {
                    job_id: scan.job_id.clone(),
                },
                &caller,
            )
            .unwrap();
        let complete = service
            .execute_scan_step(&scan.job_id, &mut cursor, budget)
            .unwrap();
        let job = service.job(&scan.job_id).unwrap();
        assert!(
            complete,
            "a cancelled scan must finish at its next batch boundary"
        );
        assert!(cursor.is_none());
        assert!(matches!(job.state, api::JobState::Cancelled));
        assert!(job.completed_at_ms.is_some());
        assert_eq!(docker.calls.load(Ordering::SeqCst), 0);
        fixture
            .database
            .read(|c| {
                let observed: Option<u64> = c.query_row(
                    "SELECT last_scan_at_ms FROM storage_scan_state WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(observed, None, "incomplete discovery must not be published");
                Ok(())
            })
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_discovery_error_finishes_scan_with_terminal_state() {
        let fixture = Fixture::new();
        let service = failure_scan_service(
            &fixture,
            devcoordinator2_api::ErrorCode::StorageUnavailable,
            "fixture_discovery_failed",
        );
        let caller = system_caller();
        let scan = service
            .scan(
                api::Scan {
                    repository_id: None,
                    idempotency_key: "permanent-discovery-error".into(),
                },
                &caller,
            )
            .unwrap();
        let scheduler =
            Arc::new(crate::maintenance::Scheduler::new(fixture.database.clone(), 1, 4).unwrap());
        service.set_maintenance_scheduler(Arc::downgrade(&scheduler));
        let (_stop, shutdown) = tokio::sync::watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(5),
            service.run_job(scan.clone(), caller.uid, shutdown),
        )
        .await
        .expect("permanent discovery failure must finish the accepted scan");
        scheduler.shutdown_and_join();
        let job = service.job(&scan.job_id).unwrap();
        assert!(matches!(job.state, api::JobState::Failed));
        assert_eq!(job.error_code.as_deref(), Some("fixture_discovery_failed"));
        let key = format!("storage:{}", scan.job_id);
        assert_eq!(
            fixture
                .database
                .read(|connection| Ok(connection.query_row(
                    "SELECT count(*) FROM coordinator_maintenance WHERE job_key=$1",
                    [&key],
                    |row| row.get::<_, u64>(0),
                )?))
                .unwrap(),
            0,
            "terminal scan failure must remove its maintenance cursor"
        );
    }

    #[test]
    fn database_unavailable_scan_error_remains_retryable() {
        let fixture = Fixture::new();
        let service = failure_scan_service(
            &fixture,
            devcoordinator2_api::ErrorCode::StorageUnavailable,
            "storage_database_unavailable",
        );
        let caller = system_caller();
        let scan = service
            .scan(
                api::Scan {
                    repository_id: None,
                    idempotency_key: "database-unavailable-scan".into(),
                },
                &caller,
            )
            .unwrap();
        let job = scan_job_for_test(service.clone(), scan.job_id.clone(), None);
        let mut saw_retry = false;
        for _ in 0..100 {
            let result = job.run(
                &mut crate::maintenance::Budget::new(Duration::from_millis(25), 256, 256 * 1024),
                None,
            );
            match result.status {
                crate::maintenance::JobStatus::Continue { .. } => {}
                crate::maintenance::JobStatus::Retry { .. } => {
                    saw_retry = true;
                    break;
                }
                status => panic!("database outage must not terminalize scan: {status:?}"),
            }
        }
        assert!(saw_retry);
        assert!(matches!(
            service.job(&scan.job_id).unwrap().state,
            api::JobState::Running
        ));
    }

    // The real storage worker, native discovery and maintenance scheduler use
    // PostgreSQL here. Only the Docker adapter pauses one bounded step; no host
    // daemon, container or live authority database participates in this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storage_scan_yields_capacity_to_waiting_cleanup_admission() {
        use std::{future::Future, task::Poll};

        let fixture = Fixture::new();
        let (entered, ready) = oneshot::channel();
        let (release, receiver) = mpsc::channel();
        let docker = Arc::new(ScanDocker {
            boundary: Mutex::new(Some((entered, receiver))),
            ..Default::default()
        });
        let service = scan_service(&fixture, docker.clone());
        let caller = system_caller();
        service.capacity.set_cap(Some(1), "fixture").unwrap();
        let scheduler =
            Arc::new(crate::maintenance::Scheduler::new(fixture.database.clone(), 1, 4).unwrap());
        service.set_maintenance_scheduler(Arc::downgrade(&scheduler));
        let scan = service
            .scan(
                api::Scan {
                    repository_id: Some("project-alpha".into()),
                    idempotency_key: "yield-scan-capacity".into(),
                },
                &caller,
            )
            .unwrap();
        let (_stop, shutdown) = watch::channel(false);
        let worker = tokio::spawn({
            let service = service.clone();
            let scan = scan.clone();
            let shutdown = shutdown.clone();
            async move { service.run_job(scan, caller.uid, shutdown).await }
        });
        tokio::time::timeout(Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
        service
            .capacity
            .register_run("cleanup-admission", caller.uid)
            .unwrap();
        let mut admission = Box::pin(service.capacity.admit_native(
            "cleanup-admission",
            "storage",
            caller.uid,
            shutdown,
        ));
        assert!(matches!(
            std::future::poll_fn(|cx| Poll::Ready(admission.as_mut().poll(cx))).await,
            Poll::Pending
        ));
        assert_eq!(service.capacity.snapshot().unwrap().waiting, 1);
        release.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(5), admission)
            .await
            .unwrap()
            .unwrap();
        let state_when_admitted = service.job(&scan.job_id).unwrap().state;
        service
            .cancel(
                api::JobReference {
                    job_id: scan.job_id.clone(),
                },
                &caller,
            )
            .unwrap();
        drop(permit);
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        service
            .capacity
            .unregister_run("cleanup-admission")
            .unwrap();
        scheduler.shutdown_and_join();
        assert!(
            matches!(state_when_admitted, api::JobState::Running),
            "cleanup must receive capacity before discovery finishes"
        );
        assert!(matches!(
            service.job(&scan.job_id).unwrap().state,
            api::JobState::Cancelled
        ));
        assert_eq!(
            docker.calls.load(Ordering::SeqCst),
            1,
            "cancelled discovery must not start another native batch"
        );
        assert_eq!(service.capacity.snapshot().unwrap().active, 0);
    }

    #[test]
    fn storage_scan_requests_are_delayed_after_a_large_run() {
        assert_eq!(scan_after_request(1_000, None), 2_000);
        assert_eq!(scan_after_request(1_000, Some(900_000)), 1_200_000);
        assert_eq!(scan_after_completion(1_000), 301_000);
    }
}
