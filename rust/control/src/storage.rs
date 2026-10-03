//! One authority for discovery, safety, policy, manual and automatic cleanup.
pub(crate) mod evidence;
pub mod fs;
mod locks;
pub mod model;
pub mod native;
mod worker;

use crate::{
    access::Caller,
    capacity::CapacityBroker,
    config::Config,
    database::{Database, DatabaseError},
    events::EventService,
    platform::Clock,
};
use devcoordinator2_api::{ErrorCode, ProtocolError, storage as api};
use model::{Locator, Record, RootRecord, StoredPlan, atom};
use rusqlite::OptionalExtension;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::Notify;

pub const SAFETY_FRESH_MS: u64 = 300_000;
const MAX_SELECTION: usize = 200;
const MAX_PLAN_ITEMS: usize = 500;

pub(crate) struct Projection {
    policies: BTreeMap<String, api::Policy>,
    active_leases: BTreeSet<String>,
    protected_resources: BTreeSet<String>,
    protected_ancestors: BTreeSet<String>,
    leased_resources: BTreeSet<String>,
    leased_ancestors: BTreeSet<String>,
    lease_use: BTreeMap<String, u64>,
    lease_resource_use: BTreeMap<String, u64>,
    lease_ancestor_use: BTreeMap<String, u64>,
    now: u64,
}

impl Projection {
    fn policy(&self, repository: Option<&str>) -> api::Policy {
        if let Some(policy) = repository.and_then(|id| self.policies.get(id)) {
            return policy.clone();
        }
        let mut policy = self.policies.get("").cloned().unwrap_or(api::Policy {
            repository_id: None,
            revision: 0,
            automatic: true,
            cache_idle_seconds: api::CACHE_IDLE_SECONDS,
            data_idle_seconds: api::DATA_IDLE_SECONDS,
            minimum_verified_backups: 2,
            inherited: false,
        });
        if let Some(id) = repository {
            policy.repository_id = Some(id.into());
            policy.revision = 0;
            policy.inherited = true;
        }
        policy
    }
}

pub fn run_mount_maintenance(
    config: &Config,
    job_id: &str,
    artifact_id: &str,
) -> Result<(), ProtocolError> {
    native::run_mount_helper(config, job_id, artifact_id)
}

#[derive(Clone)]
pub struct StorageService {
    pub(crate) database: Database,
    pub(crate) config: Arc<Config>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) backend: Arc<dyn native::Backend>,
    pub(crate) capacity: CapacityBroker,
    pub(crate) events: EventService,
    pub(crate) wake: Arc<Notify>,
    pub(crate) discovery_requested: Arc<std::sync::atomic::AtomicBool>,
}

impl StorageService {
    pub fn new(
        config: Config,
        database: Database,
        clock: Arc<dyn Clock>,
        capacity: CapacityBroker,
        events: EventService,
    ) -> Self {
        let backend = Arc::new(native::HostBackend::new(config.clone(), database.clone()));
        Self::with_backend(config, database, clock, capacity, events, backend)
    }

    pub fn with_backend(
        config: Config,
        database: Database,
        clock: Arc<dyn Clock>,
        capacity: CapacityBroker,
        events: EventService,
        backend: Arc<dyn native::Backend>,
    ) -> Self {
        Self {
            database,
            config: Arc::new(config),
            clock,
            backend,
            capacity,
            events,
            wake: Arc::new(Notify::new()),
            discovery_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn request_discovery(&self) {
        self.discovery_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }

    pub fn now_ms(&self) -> u64 {
        #[cfg(feature = "root-acceptance")]
        if self
            .config
            .unit_prefix
            .starts_with("devcoordinator2-rustint-")
            && let Ok(value) =
                std::fs::read_to_string(self.config.state_dir.join("storage-test-clock-ms"))
            && let Ok(value) = value.trim().parse::<u64>()
        {
            return value;
        }
        self.clock
            .now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000)
            .max(0) as u64
    }

    pub fn inventory(&self, request: api::List) -> Result<api::Inventory, ProtocolError> {
        let limit = request.limit.unwrap_or(50).clamp(1, 100) as usize;
        if request.query.as_ref().is_some_and(|s| s.len() > 200) {
            return Err(invalid("query_too_long"));
        }
        let query = request.query.as_ref().map(|s| s.to_lowercase());
        let mut rows = self
            .projected_records()?
            .into_values()
            .filter(|r| {
                (request.include_removed || r.removed_at_ms.is_none())
                    && request
                        .repository_id
                        .as_ref()
                        .is_none_or(|id| r.repository_id.as_ref() == Some(id))
                    && request
                        .filesystem_id
                        .as_ref()
                        .is_none_or(|id| r.filesystem_id.as_ref() == Some(id))
                    && request.kind.is_none_or(|kind| r.kind == kind)
                    && query.as_ref().is_none_or(|q| {
                        r.name.to_lowercase().contains(q)
                            || r.repository_name
                                .as_ref()
                                .is_some_and(|s| s.to_lowercase().contains(q))
                    })
            })
            .collect::<Vec<_>>();
        rows.retain(|r| request.safety.is_none_or(|s| r.safety == s));
        rows.sort_by(|a, b| {
            a.repository_name
                .cmp(&b.repository_name)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.artifact_id.cmp(&b.artifact_id))
        });
        let total = rows.len() as u64;
        let offset = request.offset as usize;
        let artifacts = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let next_offset = ((offset + artifacts.len()) < (total as usize))
            .then_some((offset + artifacts.len()) as u32);
        let (revision,last_scan_at_ms,filesystems,coverage_gaps)=self.database.call(|c| {
            c.query_row("SELECT revision,last_scan_at_ms,filesystem_json,coverage_json FROM storage_scan_state WHERE singleton=1",[],|r|Ok((super_sql_u64(r.get::<_,i64>(0)?)?,r.get::<_,Option<i64>>(1)?.map(super_sql_u64).transpose()?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).map_err(DatabaseError::from)
        }).map_err(db_error)?;
        Ok(api::Inventory {
            revision,
            artifacts,
            filesystems: parse(&filesystems)?,
            last_scan_at_ms,
            coverage_gaps: parse(&coverage_gaps)?,
            next_offset,
            total,
        })
    }

    pub fn artifact(&self, id: &str) -> Result<api::Artifact, ProtocolError> {
        self.projected_records()?
            .remove(id)
            .ok_or_else(|| not_found("artifact_not_found"))
    }

    fn projected_records(&self) -> Result<BTreeMap<String, api::Artifact>, ProtocolError> {
        let projection = self.projection()?;
        let mut rows = self
            .records()?
            .into_iter()
            .map(|(id, r)| self.project_with(r, &projection).map(|a| (id, a)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        loop {
            let mut changed = Vec::new();
            for (id, row) in &rows {
                if !row.deletable {
                    continue;
                }
                if let Some(dependency) = row
                    .dependencies
                    .iter()
                    .filter_map(|id| rows.get(id))
                    .find(|r| !r.deletable && r.removed_at_ms.is_none())
                {
                    let reason = match dependency.safety {
                        api::Safety::Protected => "dependency_protected",
                        api::Safety::InUse => "dependency_in_use",
                        _ => "dependency_needs_review",
                    };
                    changed.push((id.clone(), dependency.safety, reason.to_owned()));
                }
            }
            if changed.is_empty() {
                break;
            }
            for (id, safety, reason) in changed {
                if let Some(row) = rows.get_mut(&id) {
                    row.deletable = false;
                    row.automatic_eligible = false;
                    row.safety = safety;
                    row.reasons = vec![reason];
                }
            }
        }
        Ok(rows)
    }

    pub fn policy(&self, scope: api::Scope) -> Result<api::Policy, ProtocolError> {
        let key = scope.repository_id.clone().unwrap_or_default();
        let found = self
            .database
            .call(move |c| {
                c.query_row(
                    "SELECT policy_json FROM storage_policies WHERE scope=?1",
                    [key],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        if let Some(value) = found {
            return parse(&value);
        }
        if scope.repository_id.is_some() {
            let mut inherited = self.policy(api::Scope::default())?;
            inherited.repository_id = scope.repository_id;
            inherited.inherited = true;
            inherited.revision = 0;
            return Ok(inherited);
        }
        Ok(api::Policy {
            repository_id: None,
            revision: 0,
            automatic: true,
            cache_idle_seconds: api::CACHE_IDLE_SECONDS,
            data_idle_seconds: api::DATA_IDLE_SECONDS,
            minimum_verified_backups: 2,
            inherited: false,
        })
    }

    pub fn set_policy(
        &self,
        p: api::PolicySet,
        caller: &Caller,
    ) -> Result<api::Policy, ProtocolError> {
        if p.expected_revision >= i64::MAX as u64
            || p.cache_idle_seconds > 315_360_000
            || p.data_idle_seconds > 315_360_000
            || p.cache_idle_seconds == 0
            || p.data_idle_seconds == 0
            || p.minimum_verified_backups < 2
            || p.minimum_verified_backups > 1000
        {
            return Err(invalid("invalid_retention_bounds"));
        }
        let policy = api::Policy {
            repository_id: p.repository_id.clone(),
            revision: p.expected_revision + 1,
            automatic: p.automatic,
            cache_idle_seconds: p.cache_idle_seconds,
            data_idle_seconds: p.data_idle_seconds,
            minimum_verified_backups: p.minimum_verified_backups,
            inherited: false,
        };
        let key = p.repository_id.unwrap_or_default();
        let value = json(&policy)?;
        let actor = caller.actor();
        let now = self.now_ms();
        self.database.transaction(move |c| {
            if !key.is_empty() && !c.query_row("SELECT EXISTS(SELECT 1 FROM repositories WHERE repository_id=?1)",[&key],|r|r.get::<_,bool>(0))? { return Err(DatabaseError::Domain(invalid("repository_not_registered"))); }
            let revision=c.query_row("SELECT revision FROM storage_policies WHERE scope=?1",[&key],|r|r.get::<_,i64>(0).and_then(super_sql_u64)).optional()?.unwrap_or(0);
            if revision!=p.expected_revision { return Err(DatabaseError::Domain(conflict("policy_changed"))); }
            c.execute("INSERT INTO storage_policies VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision,policy_json=excluded.policy_json,updated_at_ms=excluded.updated_at_ms,actor=excluded.actor",rusqlite::params![key,(revision+1) as i64,value,now as i64,actor])?;
            change(c,None,"policy",&actor,now,&value)?;Ok(())
        }).map_err(db_error)?;
        self.request_discovery();
        Ok(policy)
    }

    pub fn protect(
        &self,
        p: api::Protection,
        caller: &Caller,
    ) -> Result<api::Artifact, ProtocolError> {
        let mut r = self.record(&p.artifact_id)?;
        if r.artifact.revision != p.expected_revision {
            return Err(conflict("artifact_changed"));
        }
        let _retention_lock = if let model::Locator::Evidence {
            worktree, run_id, ..
        } = &r.locator
        {
            let lock = devcoordinator2_executor_core::log_query::lock_retention(worktree)
                .map_err(|_| blocked("artifact_cleanup_in_progress"))?;
            let settings = self.context()?;
            let request = devcoordinator2_executor_core::log_query::LogPruneRequest {
                schema: 2,
                repository_id: r
                    .artifact
                    .repository_id
                    .clone()
                    .ok_or_else(|| blocked("ownership_unknown"))?,
                max_age_seconds: settings.retention_age_seconds,
                case_depth: settings.retention_depth,
                active_run_id: crate::test_logs::active_run_id(worktree)
                    .map_err(|_| unavailable("evidence_active_state_unavailable"))?,
            };
            if !devcoordinator2_executor_core::log_query::retention_runs(worktree, &request)
                .map_err(|_| unavailable("evidence_inventory_unavailable"))?
                .iter()
                .any(|run| &run.run_id == run_id)
            {
                return Err(blocked("evidence_already_removed"));
            }
            Some(lock)
        } else {
            None
        };
        r.artifact.protected = p.protected;
        self.save_changed(r, p.expected_revision, "protection", &caller.actor())?;
        self.request_discovery();
        self.artifact(&p.artifact_id)
    }

    pub fn register(
        &self,
        p: api::Register,
        caller: &Caller,
    ) -> Result<api::Artifact, ProtocolError> {
        text(&p.reason, 2000)?;
        let mut r = self.record(&p.artifact_id)?;
        if r.artifact.revision != p.expected_revision {
            return Err(conflict("artifact_changed"));
        }
        if r.artifact.removed_at_ms.is_some() {
            return Err(blocked("already_removed"));
        }
        if p.effect == api::Effect::Unknown {
            return Err(invalid("disposal_effect_required"));
        }
        let context = self.context()?;
        self.backend.validate(&r, &context, &[])?;
        if let Some(repo) = p.repository_id {
            let owner = context
                .repositories
                .iter()
                .find(|x| x.id == repo)
                .ok_or_else(|| invalid("repository_not_registered"))?;
            if r.artifact
                .repository_id
                .as_ref()
                .is_some_and(|id| id != &repo)
            {
                return Err(blocked("ownership_conflict"));
            }
            r.artifact.repository_id = Some(repo);
            r.artifact.repository_name = Some(owner.name.clone());
        }
        r.disposal_approved = true;
        r.disposal_reason = Some(p.reason);
        r.artifact.effect = p.effect;
        r.artifact.ownership = "cleanup_owned".into();
        r.blockers
            .retain(|s| s != "ownership_unknown" && s != "disposal_not_authorized");
        self.save_changed(r, p.expected_revision, "disposal", &caller.actor())?;
        self.request_discovery();
        self.artifact(&p.artifact_id)
    }

    pub fn roots(&self) -> Result<api::Roots, ProtocolError> {
        Ok(api::Roots {
            roots: self.root_records()?.into_iter().map(|r| r.root).collect(),
        })
    }

    pub fn set_root(&self, p: api::RootSet, caller: &Caller) -> Result<api::Root, ProtocolError> {
        text(&p.label, 120)?;
        if p.expected_revision >= i64::MAX as u64 {
            return Err(invalid("invalid_revision"));
        }
        if !matches!(
            p.kind,
            api::Kind::BuildOutput
                | api::Kind::DependencyCache
                | api::Kind::Backup
                | api::Kind::Unknown
        ) {
            return Err(invalid("unsupported_root_kind"));
        }
        let path = std::path::PathBuf::from(&p.path);
        native::validate_root(&path, &self.config)?;
        let (device, inode) = fs::identity(&path)?;
        let id = p
            .root_id
            .unwrap_or_else(|| stable_id("sr", p.path.as_bytes()));
        let root = api::Root {
            root_id: id.clone(),
            repository_id: p.repository_id,
            label: p.label,
            kind: p.kind,
            revision: p.expected_revision + 1,
        };
        let record = RootRecord {
            root: root.clone(),
            path,
            device,
            inode,
        };
        let encoded = json(&record)?;
        let actor = caller.actor();
        let now = self.now_ms();
        self.database.transaction(move|c| {
            let revision=c.query_row("SELECT revision FROM storage_roots WHERE root_id=?1",[&id],|r|r.get::<_,i64>(0).and_then(super_sql_u64)).optional()?.unwrap_or(0);
            if revision!=p.expected_revision { return Err(DatabaseError::Domain(conflict("root_changed"))); }
            c.execute("INSERT INTO storage_roots VALUES(?1,?2,?3,?4,?5) ON CONFLICT(root_id) DO UPDATE SET revision=excluded.revision,root_json=excluded.root_json,actor=excluded.actor,updated_at_ms=excluded.updated_at_ms",rusqlite::params![id,(revision+1) as i64,encoded,actor,now as i64])?;
            change(c,None,"root",&actor,now,"{}")?;Ok(())
        }).map_err(db_error)?;
        self.request_discovery();
        Ok(root)
    }

    pub fn plan(
        &self,
        p: api::PlanRequest,
        caller: &Caller,
    ) -> Result<api::CleanupPlan, ProtocolError> {
        if p.artifact_ids.is_empty() || p.artifact_ids.len() > MAX_SELECTION {
            return Err(invalid("selection_requires_1_to_200_artifacts"));
        }
        let records = self.records()?;
        let projection = self.projection()?;
        let mut visited = BTreeSet::new();
        let mut ordered = Vec::new();
        for id in &p.artifact_ids {
            visit(
                id,
                &records,
                &mut BTreeSet::new(),
                &mut visited,
                &mut ordered,
            )?;
        }
        // A bind-backed volume and its backing directory are one deletion scope.
        // The backing directory depends on Docker removal, so add companions before
        // ordering rather than creating a cyclic volume/directory dependency.
        let physical_keys = ordered
            .iter()
            .filter_map(|id| records.get(id).map(|r| r.resource_key.clone()))
            .collect::<BTreeSet<_>>();
        for r in records.values().filter(|r| {
            r.artifact.kind == api::Kind::BackingDirectory
                && physical_keys.contains(&r.resource_key)
        }) {
            visit(
                &r.artifact.artifact_id,
                &records,
                &mut BTreeSet::new(),
                &mut visited,
                &mut ordered,
            )?;
        }
        // A selected directory removes its known nested directories too. Bind
        // them into the reviewed plan, including their protection and effects.
        let directories = ordered
            .iter()
            .filter_map(|id| records.get(id))
            .filter(|r| matches!(r.locator, Locator::Directory { .. }))
            .map(|r| r.resource_key.clone())
            .collect::<BTreeSet<_>>();
        for r in records.values().filter(|r| {
            r.artifact.removed_at_ms.is_none()
                && matches!(r.locator, Locator::Directory { .. })
                && r.ancestor_keys.iter().any(|key| directories.contains(key))
        }) {
            visit(
                &r.artifact.artifact_id,
                &records,
                &mut BTreeSet::new(),
                &mut visited,
                &mut ordered,
            )?;
        }
        order_contained_directories(&mut ordered, &records)?;
        let mut items = Vec::new();
        let mut policies = BTreeMap::new();
        policies.insert(String::new(), self.policy(api::Scope::default())?.revision);
        let mut bound = Vec::new();
        for id in ordered {
            let r = records
                .get(&id)
                .ok_or_else(|| not_found("artifact_not_found"))?;
            let row = self.project_with(r.clone(), &projection)?;
            let mut blockers = if row.deletable {
                Vec::new()
            } else {
                row.reasons.clone()
            };
            if !p.include_persistent_data && model::persistent(row.effect) {
                blockers.push("persistent_data_not_selected".into());
            }
            if p.automatic && !row.automatic_eligible {
                blockers.push("automatic_policy_not_due".into());
            }
            let policy = projection.policy(row.repository_id.as_deref());
            policies.insert(policy.repository_id.unwrap_or_default(), policy.revision);
            items.push(api::PlanItem {
                artifact_id: id,
                revision: row.revision,
                name: row.name,
                kind: row.kind,
                effect: row.effect,
                allocated_bytes: row.allocated_bytes,
                blockers,
            });
            bound.push(r.clone());
        }
        let eligible = bound
            .iter()
            .zip(&items)
            .filter(|(_, item)| item.blockers.is_empty())
            .map(|(r, _)| r.resource_key.clone())
            .collect::<BTreeSet<_>>();
        let mut physical = BTreeSet::new();
        let total = bound
            .iter()
            .zip(&items)
            .filter(|(r, item)| {
                item.blockers.is_empty()
                    && !r.ancestor_keys.iter().any(|key| eligible.contains(key))
                    && physical.insert(r.resource_key.clone())
            })
            .map(|(_, item)| item.allocated_bytes.unwrap_or(0))
            .sum();
        let now = self.now_ms();
        let public = api::CleanupPlan {
            plan_id: new_id("sp")?,
            created_at_ms: now,
            expires_at_ms: now + SAFETY_FRESH_MS,
            ready: items.iter().all(|i| i.blockers.is_empty()),
            items,
            reclaimable_bytes: total,
            automatic: p.automatic,
            includes_persistent_data: p.include_persistent_data,
        };
        let plan = StoredPlan {
            public: public.clone(),
            records: bound,
            policies,
        };
        let id = public.plan_id.clone();
        let encoded = json(&plan)?;
        let actor = caller.actor();
        self.database
            .call(move |c| {
                c.execute(
                    "INSERT INTO storage_plans VALUES(?1,?2,?3,?4)",
                    rusqlite::params![id, encoded, now as i64, actor],
                )?;
                Ok(())
            })
            .map_err(db_error)?;
        Ok(public)
    }

    pub fn scan(&self, p: api::Scan, caller: &Caller) -> Result<api::Job, ProtocolError> {
        self.queue("scan", &p.idempotency_key, &p, caller, None)
    }

    pub fn start(&self, p: api::Start, caller: &Caller) -> Result<api::Job, ProtocolError> {
        // A lost reply is retried against the receipt before revalidating a plan
        // whose successful execution has necessarily changed its artifacts.
        let actor = caller.actor();
        let key = p.idempotency_key.clone();
        let digest = hash(json(&p)?.as_bytes());
        let existing=self.database.call(move|c|c.query_row("SELECT job_json,request_sha256 FROM storage_jobs WHERE actor=?1 AND kind='cleanup' AND idempotency_key=?2",rusqlite::params![actor,key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional().map_err(DatabaseError::from)).map_err(db_error)?;
        if let Some((value, prior)) = existing {
            if digest != prior {
                return Err(conflict("idempotency_key_reused"));
            }
            return parse(&value);
        }
        let plan = self.stored_plan(&p.plan_id)?;
        if !plan.public.ready {
            return Err(blocked("plan_has_blockers"));
        }
        if plan.public.expires_at_ms < self.now_ms() {
            return Err(conflict("plan_expired"));
        }
        for r in &plan.records {
            let current = self.record(&r.artifact.artifact_id)?;
            if current.artifact.revision != r.artifact.revision
                || current.fingerprint != r.fingerprint
                || current.resource_key != r.resource_key
            {
                return Err(conflict("artifact_changed"));
            }
        }
        self.queue(
            "cleanup",
            &p.idempotency_key,
            &p,
            caller,
            Some(p.plan_id.clone()),
        )
    }

    pub fn job(&self, id: &str) -> Result<api::Job, ProtocolError> {
        let id = id.to_owned();
        let value = self
            .database
            .call(move |c| {
                c.query_row(
                    "SELECT job_json FROM storage_jobs WHERE job_id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        parse(&value.ok_or_else(|| not_found("job_not_found"))?)
    }

    pub fn cancel(&self, p: api::JobReference, caller: &Caller) -> Result<api::Job, ProtocolError> {
        let actor = caller.actor();
        let now = self.now_ms();
        let job = self
            .database
            .transaction(move |c| {
                let value = c
                    .query_row(
                        "SELECT job_json FROM storage_jobs WHERE job_id=?1",
                        [&p.job_id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| DatabaseError::Domain(not_found("job_not_found")))?;
                let mut job: api::Job = parse(&value).map_err(DatabaseError::Domain)?;
                match job.state {
                    api::JobState::Queued => {
                        job.state = api::JobState::Cancelled;
                        job.completed_at_ms = Some(now);
                    }
                    api::JobState::Running => job.state = api::JobState::Cancelling,
                    _ => return Ok(job),
                }
                c.execute(
                    "UPDATE storage_jobs SET state=?1,job_json=?2 WHERE job_id=?3",
                    rusqlite::params![
                        atom(&job.state),
                        json(&job).map_err(DatabaseError::Domain)?,
                        p.job_id
                    ],
                )?;
                change(c, None, "cancel", &actor, now, "{}")?;
                Ok(job)
            })
            .map_err(db_error)?;
        if matches!(job.state, api::JobState::Cancelled) {
            self.finish(&job.job_id, api::JobState::Cancelled, None)?;
            return self.job(&job.job_id);
        }
        self.wake.notify_one();
        Ok(job)
    }

    pub fn history(&self, p: api::HistoryRequest) -> Result<api::History, ProtocolError> {
        let limit = p.limit.unwrap_or(20).clamp(1, 50) as usize;
        let before = p.before_ms.unwrap_or(i64::MAX as u64);
        let rows=self.database.call(move|c| {
            let mut q=c.prepare("SELECT job_json FROM storage_jobs WHERE created_at_ms < ?1 AND (?2 IS NULL OR EXISTS(SELECT 1 FROM storage_item_receipts i WHERE i.job_id=storage_jobs.job_id AND i.artifact_id=?2)) ORDER BY created_at_ms DESC,job_id DESC LIMIT ?3")?;
            Ok(q.query_map(rusqlite::params![before as i64,p.artifact_id,(limit+1) as u32],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(db_error)?;
        let more = rows.len() > limit;
        let jobs = rows
            .into_iter()
            .take(limit)
            .map(|s| parse::<api::Job>(&s))
            .collect::<Result<Vec<_>, _>>()?;
        let next_before_ms = more.then(|| jobs.last().map(|j| j.created_at_ms)).flatten();
        Ok(api::History {
            jobs,
            next_before_ms,
        })
    }

    pub fn lease(&self, p: api::LeaseSet, caller: &Caller) -> Result<api::Lease, ProtocolError> {
        if p.artifact_ids.is_empty()
            || p.artifact_ids.len() > MAX_SELECTION
            || !(1..=86_400).contains(&p.duration_seconds)
        {
            return Err(invalid("invalid_lease"));
        }
        let resources = p
            .artifact_ids
            .iter()
            .map(|id| self.record(id).map(|r| (r.resource_key, r.ancestor_keys)))
            .collect::<Result<Vec<_>, _>>()?;
        let lease = api::Lease {
            lease_id: p.lease_id.unwrap_or(new_id("sl")?),
            artifact_ids: p.artifact_ids,
            expires_at_ms: self.now_ms().saturating_add(p.duration_seconds * 1000),
        };
        let copy = lease.clone();
        let value = json(&lease)?;
        let actor = caller.actor();
        let now = self.now_ms();
        self.database.transaction(move|c| {
            for (resource,ancestors) in resources {locks::ensure_tree_editable(c,&resource,&ancestors,None)?;}
            let old=c.query_row("SELECT lease_json FROM storage_leases WHERE lease_id=?1",[&copy.lease_id],|r|r.get::<_,String>(0)).optional()?;
            if let Some(old)=old {remember_lease_use(c,&parse::<api::Lease>(&old).map_err(DatabaseError::Domain)?,now)?;}
            remember_lease_use(c,&copy,now)?;
            c.execute("INSERT INTO storage_leases VALUES(?1,?2,?3,?4) ON CONFLICT(lease_id) DO UPDATE SET lease_json=excluded.lease_json,expires_at_ms=excluded.expires_at_ms",rusqlite::params![copy.lease_id,actor,value,copy.expires_at_ms as i64])?;Ok(())
        }).map_err(db_error)?;
        self.wake.notify_one();
        Ok(lease)
    }

    pub fn release_lease(
        &self,
        p: api::LeaseRelease,
        caller: &Caller,
    ) -> Result<api::Lease, ProtocolError> {
        let actor = caller.actor();
        let now = self.now_ms();
        let value = self
            .database
            .transaction(move |c| {
                let value = c
                    .query_row(
                        "SELECT lease_json FROM storage_leases WHERE lease_id=?1",
                        [&p.lease_id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| DatabaseError::Domain(not_found("lease_not_found")))?;
                let mut lease: api::Lease = parse(&value).map_err(DatabaseError::Domain)?;
                remember_lease_use(c, &lease, now)?;
                lease.expires_at_ms = now;
                c.execute(
                    "UPDATE storage_leases SET expires_at_ms=?1,lease_json=?2 WHERE lease_id=?3",
                    rusqlite::params![
                        now as i64,
                        json(&lease).map_err(DatabaseError::Domain)?,
                        p.lease_id
                    ],
                )?;
                change(c, None, "lease_released", &actor, now, "{}")?;
                Ok(lease)
            })
            .map_err(db_error)?;
        self.wake.notify_one();
        Ok(value)
    }

    fn queue<T: Serialize>(
        &self,
        kind: &str,
        key: &str,
        request: &T,
        caller: &Caller,
        plan_id: Option<String>,
    ) -> Result<api::Job, ProtocolError> {
        text(key, 200)?;
        let encoded = json(request)?;
        let digest = hash(encoded.as_bytes());
        let actor = caller.actor();
        let kind = kind.to_owned();
        let key = key.to_owned();
        let uid = caller.uid;
        let now = self.now_ms();
        let job = api::Job {
            job_id: new_id("sj")?,
            kind: kind.clone(),
            state: api::JobState::Queued,
            created_at_ms: now,
            started_at_ms: None,
            completed_at_ms: None,
            actor: actor.clone(),
            plan_id,
            receipts: Vec::new(),
            reclaimed_bytes: 0,
            unmeasured_items: 0,
            error_code: None,
        };
        let value = json(&job)?;
        let result=self.database.transaction(move|c| {
            if let Some((prior,prior_digest))=c.query_row("SELECT job_json,request_sha256 FROM storage_jobs WHERE actor=?1 AND kind=?2 AND idempotency_key=?3",rusqlite::params![actor,kind,key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()? {
                if prior_digest!=digest {return Err(DatabaseError::Domain(conflict("idempotency_key_reused")));}
                return parse(&prior).map_err(DatabaseError::Domain);
            }
            c.execute("INSERT INTO storage_jobs VALUES(?1,?2,'queued',?3,?4,?5,?6,?7,?8,?9)",rusqlite::params![job.job_id,kind,key,digest,actor,uid,value,encoded,now as i64])?;Ok(job)
        }).map_err(db_error)?;
        self.wake.notify_one();
        Ok(result)
    }

    pub(crate) fn record(&self, id: &str) -> Result<Record, ProtocolError> {
        let id = id.to_owned();
        let value = self
            .database
            .call(move |c| {
                c.query_row(
                    "SELECT record_json FROM storage_artifacts WHERE artifact_id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        parse(&value.ok_or_else(|| not_found("artifact_not_found"))?)
    }

    pub(crate) fn records(&self) -> Result<BTreeMap<String, Record>, ProtocolError> {
        let rows = self
            .database
            .call(|c| {
                let mut q =
                    c.prepare("SELECT record_json FROM storage_artifacts ORDER BY artifact_id")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(db_error)?;
        rows.into_iter()
            .map(|s| parse::<Record>(&s).map(|r| (r.artifact.artifact_id.clone(), r)))
            .collect()
    }

    pub(crate) fn root_records(&self) -> Result<Vec<RootRecord>, ProtocolError> {
        let rows = self
            .database
            .call(|c| {
                let mut q = c.prepare("SELECT root_json FROM storage_roots ORDER BY root_id")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(db_error)?;
        rows.into_iter().map(|s| parse(&s)).collect()
    }

    pub(crate) fn stored_plan(&self, id: &str) -> Result<StoredPlan, ProtocolError> {
        let id = id.to_owned();
        let value = self
            .database
            .call(move |c| {
                c.query_row(
                    "SELECT plan_json FROM storage_plans WHERE plan_id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(DatabaseError::from)
            })
            .map_err(db_error)?;
        parse(&value.ok_or_else(|| not_found("plan_not_found"))?)
    }

    pub(crate) fn save_changed(
        &self,
        r: Record,
        expected: u64,
        kind: &str,
        actor: &str,
    ) -> Result<(), ProtocolError> {
        self.save_changed_for_job(r, expected, kind, actor, None)
    }

    pub(crate) fn save_changed_for_job(
        &self,
        mut r: Record,
        expected: u64,
        kind: &str,
        actor: &str,
        job: Option<&str>,
    ) -> Result<(), ProtocolError> {
        r.artifact.revision = expected + 1;
        let resource = r.resource_key.clone();
        let ancestors = r.ancestor_keys.clone();
        let job = job.map(str::to_owned);
        let id = r.artifact.artifact_id.clone();
        let now = self.now_ms();
        let actor = actor.to_owned();
        let kind = kind.to_owned();
        self.database.transaction(move|c| {
            locks::ensure_tree_editable(c,&resource,&ancestors,job.as_deref())?;
            r.update_sequence = next_sequence(c)?;
            let value = json(&r).map_err(DatabaseError::Domain)?;
            let changed=c.execute("UPDATE storage_artifacts SET revision=?1,record_json=?2,updated_at_ms=?3 WHERE artifact_id=?4 AND revision=?5",rusqlite::params![(expected+1) as i64,value,now as i64,id,expected as i64])?;
            if changed!=1 {return Err(DatabaseError::Domain(conflict("artifact_changed")));}
            change(c,Some(&id),&kind,&actor,now,"{}")?;Ok(())
        }).map_err(db_error)
    }

    pub(crate) fn save_job(&self, job: &api::Job) -> Result<(), ProtocolError> {
        let id = job.job_id.clone();
        let state = atom(&job.state);
        let value = json(job)?;
        self.database
            .call(move |c| {
                c.execute(
                    "UPDATE storage_jobs SET state=?1,job_json=?2 WHERE job_id=?3",
                    rusqlite::params![state, value, id],
                )?;
                Ok(())
            })
            .map_err(db_error)
    }

    pub(crate) fn projection(&self) -> Result<Projection, ProtocolError> {
        let now = self.now_ms();
        self.database
            .call(move |c| {
                let mut projection = Projection {
                    policies: BTreeMap::new(),
                    active_leases: BTreeSet::new(),
                    protected_resources: BTreeSet::new(),
                    protected_ancestors: BTreeSet::new(),
                    leased_resources: BTreeSet::new(),
                    leased_ancestors: BTreeSet::new(),
                    lease_use: BTreeMap::new(),
                    lease_resource_use: BTreeMap::new(),
                    lease_ancestor_use: BTreeMap::new(),
                    now,
                };
                let mut q=c.prepare("SELECT record_json FROM storage_artifacts WHERE removed_at_ms IS NULL AND json_extract(record_json,'$.artifact.protected')=1")?;
                for value in q.query_map([],|r|r.get::<_,String>(0))? {
                    let record:Record=parse(&value?).map_err(DatabaseError::Domain)?;
                    projection.protected_resources.insert(record.resource_key);
                    projection.protected_ancestors.extend(record.ancestor_keys);
                }
                let mut q = c.prepare("SELECT scope,policy_json FROM storage_policies")?;
                for row in
                    q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                {
                    let (scope, value) = row?;
                    projection
                        .policies
                        .insert(scope, parse(&value).map_err(DatabaseError::Domain)?);
                }
                let mut q = c.prepare("SELECT lease_json FROM storage_leases")?;
                for row in q.query_map([], |r| r.get::<_, String>(0))? {
                    let lease: api::Lease = parse(&row?).map_err(DatabaseError::Domain)?;
                    for id in lease.artifact_ids {
                        if lease.expires_at_ms > now {
                            projection.active_leases.insert(id.clone());
                        }
                        let used = projection.lease_use.entry(id).or_default();
                        *used = (*used).max(lease.expires_at_ms.min(now));
                    }
                }
                let mut q=c.prepare("SELECT record_json FROM storage_artifacts WHERE removed_at_ms IS NULL")?;
                for value in q.query_map([],|r|r.get::<_,String>(0))? {
                    let record:Record=parse(&value?).map_err(DatabaseError::Domain)?;
                    if projection.active_leases.contains(&record.artifact.artifact_id) {projection.leased_resources.insert(record.resource_key.clone());projection.leased_ancestors.extend(record.ancestor_keys.iter().cloned());}
                    if let Some(at)=projection.lease_use.get(&record.artifact.artifact_id) {
                        let latest=projection.lease_resource_use.entry(record.resource_key).or_default();
                        *latest=(*latest).max(*at);
                        for key in record.ancestor_keys {let latest=projection.lease_ancestor_use.entry(key).or_default();*latest=(*latest).max(*at);}
                    }
                }
                Ok(projection)
            })
            .map_err(db_error)
    }

    pub(crate) fn project_with(
        &self,
        r: Record,
        projection: &Projection,
    ) -> Result<api::Artifact, ProtocolError> {
        let policy = projection.policy(r.artifact.repository_id.as_deref());
        let now = projection.now;
        let shared_protection = projection.protected_resources.contains(&r.resource_key)
            || projection.protected_ancestors.contains(&r.resource_key)
            || r.ancestor_keys
                .iter()
                .any(|key| projection.protected_resources.contains(key));
        let shared_lease = projection.leased_resources.contains(&r.resource_key)
            || projection.leased_ancestors.contains(&r.resource_key)
            || r.ancestor_keys
                .iter()
                .any(|key| projection.leased_resources.contains(key));
        let shared_use = std::iter::once(&r.resource_key)
            .chain(&r.ancestor_keys)
            .filter_map(|key| projection.lease_resource_use.get(key))
            .copied()
            .max()
            .max(projection.lease_ancestor_use.get(&r.resource_key).copied());
        let mut a = r.artifact;
        a.last_used_at_ms = a
            .last_used_at_ms
            .max(projection.lease_use.get(&a.artifact_id).copied())
            .max(shared_use);
        a.accounting_id = hash(r.resource_key.as_bytes());
        let idle = if matches!(
            a.kind,
            api::Kind::BuildOutput | api::Kind::DependencyCache | api::Kind::BuildCache
        ) {
            policy.cache_idle_seconds
        } else {
            policy.data_idle_seconds
        };
        a.eligible_at_ms = Some(
            a.last_used_at_ms
                .unwrap_or(a.observed_since_ms)
                .max(a.observed_since_ms)
                .saturating_add(idle.saturating_mul(1000)),
        );
        // Pins and leases are authoritative database state. A scan that raced a
        // release must not leave their old derived reason stuck in the inventory.
        let mut reasons = r
            .blockers
            .into_iter()
            .filter(|reason| {
                !matches!(
                    reason.as_str(),
                    "active_lease" | "explicitly_protected" | "protected_shared_data"
                )
            })
            .collect::<Vec<_>>();
        if !r.disposal_approved && r.discovered_effect == api::Effect::Unknown {
            reasons.push("ownership_unknown".into());
        }
        if a.protected {
            reasons.insert(0, "explicitly_protected".into());
        }
        if shared_protection && !a.protected {
            reasons.insert(0, "protected_shared_data".into());
        }
        if a.removed_at_ms.is_some() {
            reasons.insert(0, "already_removed".into());
        }
        if shared_lease || projection.active_leases.contains(&a.artifact_id) {
            reasons.insert(0, "active_lease".into());
        }
        if a.verified_at_ms
            .is_none_or(|at| now.saturating_sub(at) > SAFETY_FRESH_MS)
        {
            reasons.push("checks_expired".into());
        }
        reasons.sort();
        reasons.dedup();
        a.safety = if a.protected
            || shared_protection
            || reasons
                .iter()
                .any(|s| matches!(s.as_str(), "required_recovery" | "required_evidence"))
        {
            api::Safety::Protected
        } else if reasons.iter().any(|s| {
            matches!(
                s.as_str(),
                "current_deployment"
                    | "active_consumer"
                    | "active_process"
                    | "active_lease"
                    | "required_recovery"
                    | "active_worktree"
                    | "active_evidence"
            )
        }) {
            api::Safety::InUse
        } else if reasons.iter().all(|s| s == "evidence_retention_pending") && !reasons.is_empty() {
            api::Safety::Observing
        } else if !reasons.is_empty() {
            api::Safety::NeedsReview
        } else {
            api::Safety::Safe
        };
        a.deletable = a.safety == api::Safety::Safe && a.removed_at_ms.is_none();
        a.automatic_eligible =
            a.deletable && policy.automatic && a.eligible_at_ms.is_some_and(|at| at <= now);
        if a.kind == api::Kind::Evidence {
            a.automatic_eligible = false;
            a.eligible_at_ms = None;
        }
        a.reasons = if reasons.is_empty() {
            vec![
                if a.effect == api::Effect::Rebuildable {
                    "unused_rebuildable"
                } else if r.disposal_approved {
                    "retired_disposal_approved"
                } else {
                    "unused_verified"
                }
                .into(),
            ]
        } else {
            reasons
        };
        Ok(a)
    }
}

fn remember_lease_use(
    c: &rusqlite::Transaction<'_>,
    lease: &api::Lease,
    now: u64,
) -> Result<(), DatabaseError> {
    for id in &lease.artifact_ids {
        let value = c
            .query_row(
                "SELECT record_json FROM storage_artifacts WHERE artifact_id=?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let Some(value) = value else {
            continue;
        };
        let mut record: Record = parse(&value).map_err(DatabaseError::Domain)?;
        if record.artifact.removed_at_ms.is_some() {
            continue;
        }
        locks::ensure_tree_editable(c, &record.resource_key, &record.ancestor_keys, None)?;
        let use_at = lease.expires_at_ms.min(now);
        if record.artifact.last_used_at_ms.is_none_or(|at| at < use_at) {
            record.artifact.last_used_at_ms = Some(use_at);
            record.artifact.revision += 1;
            record.update_sequence = next_sequence(c)?;
            c.execute("UPDATE storage_artifacts SET revision=?1,record_json=?2,updated_at_ms=?3 WHERE artifact_id=?4",rusqlite::params![record.artifact.revision as i64,json(&record).map_err(DatabaseError::Domain)?,now as i64,id])?;
        }
    }
    Ok(())
}

fn order_contained_directories(
    ordered: &mut Vec<String>,
    records: &BTreeMap<String, Record>,
) -> Result<(), ProtocolError> {
    let mut pending = std::mem::take(ordered);
    while !pending.is_empty() {
        let next = pending
            .iter()
            .position(|id| {
                let record = &records[id];
                pending.iter().all(|other| {
                    if other == id {
                        return true;
                    }
                    let parent = &records[other];
                    !(record.artifact.dependencies.contains(other)
                        || (matches!(record.locator, Locator::Directory { .. })
                            && matches!(parent.locator, Locator::Directory { .. })
                            && record.ancestor_keys.contains(&parent.resource_key)))
                })
            })
            .ok_or_else(|| blocked("dependency_cycle"))?;
        ordered.push(pending.remove(next));
    }
    Ok(())
}

fn visit(
    id: &str,
    records: &BTreeMap<String, Record>,
    visiting: &mut BTreeSet<String>,
    done: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
) -> Result<(), ProtocolError> {
    if done.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id.into()) {
        return Err(blocked("dependency_cycle"));
    }
    let r = records
        .get(id)
        .ok_or_else(|| not_found("dependency_not_found"))?;
    if r.artifact.removed_at_ms.is_some() {
        visiting.remove(id);
        done.insert(id.into());
        return Ok(());
    }
    for child in &r.artifact.dependencies {
        visit(child, records, visiting, done, ordered)?;
    }
    visiting.remove(id);
    done.insert(id.into());
    ordered.push(id.into());
    if ordered.len() > MAX_PLAN_ITEMS {
        return Err(blocked("cleanup_group_too_large"));
    }
    Ok(())
}

fn next_sequence(c: &rusqlite::Transaction<'_>) -> Result<u64, DatabaseError> {
    c.execute(
        "UPDATE storage_scan_state SET revision=revision+1 WHERE singleton=1",
        [],
    )?;
    let sequence = c.query_row(
        "SELECT revision FROM storage_scan_state WHERE singleton=1",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(super_sql_u64(sequence)?)
}

fn change(
    c: &rusqlite::Transaction<'_>,
    id: Option<&str>,
    kind: &str,
    actor: &str,
    now: u64,
    value: &str,
) -> Result<(), DatabaseError> {
    c.execute("INSERT INTO storage_changes(artifact_id,kind,actor,created_at_ms,change_json) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![id,kind,actor,now as i64,value])?;
    Ok(())
}
pub(crate) fn parse<T: DeserializeOwned>(value: &str) -> Result<T, ProtocolError> {
    serde_json::from_str(value).map_err(|_| unavailable("stored_storage_record_invalid"))
}
pub(crate) fn json<T: Serialize>(value: &T) -> Result<String, ProtocolError> {
    serde_json::to_string(value).map_err(|_| unavailable("storage_encoding_failed"))
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn stable_id(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", &hash(bytes)[..24])
}
pub(crate) fn new_id(prefix: &str) -> Result<String, ProtocolError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| unavailable("random_source_unavailable"))?;
    Ok(stable_id(prefix, &bytes))
}
fn text(value: &str, max: usize) -> Result<(), ProtocolError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        Err(invalid("invalid_text"))
    } else {
        Ok(())
    }
}
pub(crate) fn invalid(code: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, code)
}
pub(crate) fn blocked(code: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::StorageBlocked, code)
}
pub(crate) fn conflict(code: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::StorageConflict, code)
}
pub(crate) fn unavailable(code: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::StorageUnavailable, code)
}
pub(crate) fn not_found(code: &str) -> ProtocolError {
    ProtocolError::new(ErrorCode::StorageNotFound, code)
}
pub(crate) fn db_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(e) => e,
        _ => unavailable("storage_database_unavailable"),
    }
}

fn super_sql_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}
