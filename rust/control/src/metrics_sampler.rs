//! Host-wide metric sampling, attribution, storage ownership, and alerts.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use devcoordinator2_api::results::{HealthHost, HostReconciliation, MetricSample};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use tokio::sync::watch;

use crate::alerts::{AlertEngine, Condition};
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::deployment_files::DeploymentFiles;
use crate::docker::{DockerControl, ExactContainerId};
use crate::inventory::ContainerInventory;
use crate::metrics::{Aggregate, MetricsStore};
use crate::metrics_source::{CgroupStats, HostMetricSource, MetricSource};
use crate::platform::{Clock, HostMonotonicClock, MonotonicClock};
use crate::systemd::SystemdControl;

pub const SAMPLE_SECONDS: u32 = 15;
pub const STORAGE_SECONDS: u32 = 300;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SubjectKey {
    pub kind: String,
    pub id: String,
}

impl SubjectKey {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubjectMeta {
    pub repository_id: Option<String>,
    pub deployment_id: Option<String>,
    pub component: Option<String>,
    pub component_type: Option<String>,
    pub name: Option<String>,
    pub image: Option<String>,
    pub state: Option<String>,
    pub binding: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SamplerSnapshot {
    pub host: HealthHost,
    pub current: BTreeMap<SubjectKey, MetricSample>,
    pub storage: BTreeMap<SubjectKey, BTreeMap<String, u64>>,
    pub meta: BTreeMap<SubjectKey, SubjectMeta>,
}

#[derive(Clone)]
pub struct MetricSampler {
    inner: Arc<Inner>,
}

struct Inner {
    config: Arc<Config>,
    database: Database,
    docker: Arc<dyn DockerControl>,
    systemd: Arc<dyn SystemdControl>,
    source: Arc<dyn MetricSource>,
    inventory: ContainerInventory,
    metrics: MetricsStore,
    alerts: AlertEngine,
    files: DeploymentFiles,
    monotonic: Arc<dyn MonotonicClock>,
    state: Mutex<Option<SamplerSnapshot>>,
    working: Mutex<Working>,
    storage_requested: AtomicBool,
}

#[derive(Default)]
struct Working {
    minute: Option<String>,
    aggregates: BTreeMap<(String, String, String), Aggregate>,
    previous: BTreeMap<SubjectKey, (u64, f64)>,
    previous_host: Option<(u64, u64)>,
    restart_history: BTreeMap<String, Vec<(f64, u32)>>,
    last_expire: f64,
}

#[derive(Clone)]
struct Subject {
    key: SubjectKey,
    meta: SubjectMeta,
    cgroup: Option<PathBuf>,
}

#[derive(Clone)]
struct ComponentRecord {
    deployment_id: String,
    name: String,
    kind: String,
    desired_state: String,
    state: String,
    binding_kind: Option<String>,
    binding_identity: Option<String>,
}

#[derive(Clone)]
struct DeploymentRecord {
    deployment_id: String,
    repository_id: String,
}

#[derive(Clone)]
struct WorktreeRecord {
    worktree_id: String,
    repository_id: String,
    path: PathBuf,
}

type RuntimeRecords = (
    Vec<WorktreeRecord>,
    Vec<DeploymentRecord>,
    Vec<ComponentRecord>,
);

impl MetricSampler {
    pub fn new(
        config: Config,
        database: Database,
        docker: Arc<dyn DockerControl>,
        systemd: Arc<dyn SystemdControl>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let monotonic = Arc::new(HostMonotonicClock::default());
        let source = Arc::new(HostMetricSource::new(Arc::clone(&docker)));
        Self::with_adapters(config, database, docker, systemd, source, clock, monotonic)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_adapters(
        config: Config,
        database: Database,
        docker: Arc<dyn DockerControl>,
        systemd: Arc<dyn SystemdControl>,
        source: Arc<dyn MetricSource>,
        clock: Arc<dyn Clock>,
        monotonic: Arc<dyn MonotonicClock>,
    ) -> Self {
        let metrics = MetricsStore::with_clock(database.clone(), Arc::clone(&clock));
        let alerts =
            AlertEngine::with_clocks(database.clone(), Arc::clone(&clock), Arc::clone(&monotonic));
        Self {
            inner: Arc::new(Inner {
                inventory: ContainerInventory::new(
                    database.clone(),
                    config.unit_prefix.clone(),
                    Arc::clone(&docker),
                ),
                files: DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
                config: Arc::new(config),
                database,
                docker,
                systemd,
                source,
                metrics,
                alerts,
                monotonic,
                state: Mutex::new(None),
                working: Mutex::new(Working::default()),
                storage_requested: AtomicBool::new(true),
            }),
        }
    }

    pub fn metrics(&self) -> &MetricsStore {
        &self.inner.metrics
    }

    pub fn alerts(&self) -> &AlertEngine {
        &self.inner.alerts
    }

    pub fn inventory(&self) -> &ContainerInventory {
        &self.inner.inventory
    }

    pub fn request_storage(&self) {
        self.inner.storage_requested.store(true, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> Result<SamplerSnapshot, ProtocolError> {
        if self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
        {
            self.tick()?;
        }
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::InternalError, "health sample is unavailable")
            })
    }

    pub fn tick(&self) -> Result<(), ProtocolError> {
        let mut working = self
            .inner
            .working
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let minute = self.inner.metrics.minute()?;
        if working
            .minute
            .as_deref()
            .is_some_and(|previous| previous != minute)
        {
            self.flush_locked(&mut working)?;
        }
        working.minute = Some(minute);
        let now_mono = self.inner.monotonic.seconds();
        let (busy, total) = self.inner.source.host_cpu_ticks();
        let host_cpu = working
            .previous_host
            .map_or(0.0, |(previous_busy, previous_total)| {
                if total > previous_total {
                    100.0 * busy.saturating_sub(previous_busy) as f64
                        / total.saturating_sub(previous_total) as f64
                } else {
                    0.0
                }
            });
        working.previous_host = Some((busy, total));
        let memory = self.inner.source.host_memory();
        let load = self.inner.source.host_load();
        let filesystem = self.inner.source.filesystem(Path::new("/"));
        record(&mut working, "host", "host", "cpu_percent", host_cpu);
        record(
            &mut working,
            "host",
            "host",
            "memory_used",
            memory.used as f64,
        );
        record(&mut working, "host", "host", "load_1", load.0);

        let subjects = self.subjects()?;
        let mut current = BTreeMap::new();
        let mut meta = BTreeMap::new();
        let mut repositories: BTreeMap<String, MetricSample> = BTreeMap::new();
        let mut managed_cpu = 0.0;
        let mut managed_memory = 0_u64;
        for subject in subjects {
            meta.insert(subject.key.clone(), subject.meta.clone());
            let Some(stats) = self.inner.source.cgroup_stats(subject.cgroup.as_deref()) else {
                continue;
            };
            let sample = delta_sample(
                &mut working,
                &subject.key,
                stats,
                now_mono,
                self.inner.source.logical_cpus(),
            );
            record_sample(&mut working, &subject.key, &sample);
            current.insert(subject.key.clone(), sample.clone());
            if subject.key.kind == "component"
                && subject
                    .meta
                    .component_type
                    .as_deref()
                    .is_some_and(|kind| matches!(kind, "docker" | "postgres" | "compose"))
            {
                continue;
            }
            if let Some(repository_id) = &subject.meta.repository_id {
                let total = repositories
                    .entry(repository_id.clone())
                    .or_insert_with(empty_metric);
                total.cpu_percent += sample.cpu_percent;
                total.memory_bytes = total.memory_bytes.saturating_add(sample.memory_bytes);
                total.pids = total.pids.saturating_add(sample.pids);
                managed_cpu += sample.cpu_percent;
                managed_memory = managed_memory.saturating_add(sample.memory_bytes);
            }
        }
        let daemon = self
            .inner
            .source
            .cgroup_stats(self.inner.source.own_cgroup().as_deref())
            .map(|stats| {
                delta_sample(
                    &mut working,
                    &SubjectKey::new("daemon", "daemon"),
                    stats,
                    now_mono,
                    self.inner.source.logical_cpus(),
                )
            })
            .unwrap_or_else(empty_metric);
        let daemon_key = SubjectKey::new("daemon", "daemon");
        record_sample(&mut working, &daemon_key, &daemon);
        current.insert(daemon_key, daemon.clone());
        for (repository_id, sample) in repositories {
            let key = SubjectKey::new("repository", repository_id);
            record_sample(&mut working, &key, &sample);
            current.insert(key, sample);
        }
        let other = MetricSample {
            cpu_percent: (host_cpu - managed_cpu - daemon.cpu_percent).max(0.0),
            memory_bytes: memory
                .used
                .saturating_sub(managed_memory)
                .saturating_sub(daemon.memory_bytes),
            ..empty_metric()
        };
        let other_key = SubjectKey::new("other", "other");
        record_sample(&mut working, &other_key, &other);
        current.insert(other_key, other.clone());
        let host = HealthHost {
            cpu_percent: round(host_cpu, 2),
            memory_total: memory.total,
            memory_used: memory.used,
            memory_available: memory.available,
            swap_total: memory.swap_total,
            swap_used: memory.swap_total.saturating_sub(memory.swap_free),
            load_1: load.0,
            load_5: load.1,
            load_15: load.2,
            fs_size: filesystem.size,
            fs_free: filesystem.free,
            fs_used: filesystem.used,
            ncpu: self.inner.source.logical_cpus(),
            reconciliation: HostReconciliation {
                managed_cpu_percent: round(managed_cpu, 2),
                daemon_cpu_percent: round(daemon.cpu_percent, 2),
                other_cpu_percent: round(other.cpu_percent, 2),
                managed_memory,
                daemon_memory: daemon.memory_bytes,
                other_memory: other.memory_bytes,
            },
        };
        let existing_storage = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|snapshot| snapshot.storage.clone())
            .unwrap_or_default();
        let snapshot = SamplerSnapshot {
            host: host.clone(),
            current,
            storage: existing_storage,
            meta,
        };
        *self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot.clone());
        self.evaluate_alerts(&host, &snapshot, &mut working, now_mono)?;
        if now_mono - working.last_expire > 3_600.0 {
            working.last_expire = now_mono;
            let _ = self.inner.metrics.expire();
        }
        Ok(())
    }

    pub fn storage_tick(&self) -> Result<(), ProtocolError> {
        let mut working = self
            .inner
            .working
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.storage_requested.store(false, Ordering::SeqCst);
        let (repositories, worktrees, deployments, components, observed) =
            self.storage_records()
                .inspect_err(|_| self.request_storage())?;
        let mut storage = BTreeMap::new();
        let mut per_repository = repositories
            .iter()
            .map(|(id, _)| (id.clone(), repository_storage()))
            .collect::<BTreeMap<_, _>>();
        for worktree in &worktrees {
            let Some(buckets) = per_repository.get_mut(&worktree.repository_id) else {
                continue;
            };
            if let Some(size) = self
                .inner
                .source
                .directory_size(&worktree.path, StdDuration::from_secs(120))
            {
                add(buckets, "checkout", size);
            }
            if let Some(size) = self.inner.source.directory_size(
                &worktree.path.join(".devcoordinator/test"),
                StdDuration::from_secs(60),
            ) {
                add(buckets, "test_scratch", size);
                storage.insert(
                    SubjectKey::new("worktree", &worktree.worktree_id),
                    BTreeMap::from([("test_scratch".into(), size)]),
                );
            }
        }
        let deployment_map = deployments
            .iter()
            .map(|deployment| (deployment.deployment_id.as_str(), deployment))
            .collect::<HashMap<_, _>>();
        for deployment in &deployments {
            if let Some(size) = self.inner.source.directory_size(
                &self
                    .inner
                    .config
                    .deployments_dir()
                    .join(&deployment.deployment_id),
                StdDuration::from_secs(60),
            ) {
                if let Some(buckets) = per_repository.get_mut(&deployment.repository_id) {
                    add(buckets, "deployment_artifacts", size);
                }
                storage.insert(
                    SubjectKey::new("deployment", &deployment.deployment_id),
                    BTreeMap::from([("artifacts".into(), size)]),
                );
            }
        }
        let container_sizes = self.inner.source.container_sizes();
        let shared = self.inner.source.docker_shared_sizes();
        let mut volume_owners = BTreeMap::new();
        for component in &components {
            let Some(deployment) = deployment_map.get(component.deployment_id.as_str()) else {
                continue;
            };
            let prefix = format!(
                "devcoordinator2-{}-{}-",
                component.deployment_id, component.name
            );
            for volume in shared
                .volumes
                .keys()
                .filter(|name| name.starts_with(&prefix))
            {
                volume_owners.insert(
                    volume.clone(),
                    (deployment.repository_id.clone(), component.kind.clone()),
                );
            }
            if component.binding_kind.as_deref() == Some("container")
                && let Some(identity) = &component.binding_identity
                && let Some(size) = container_sizes.get(identity)
            {
                if let Some(buckets) = per_repository.get_mut(&deployment.repository_id) {
                    add(buckets, "container_layers", *size);
                }
                storage
                    .entry(SubjectKey::new(
                        "component",
                        format!("{}/{}", component.deployment_id, component.name),
                    ))
                    .or_insert_with(BTreeMap::new)
                    .insert("container_layer".into(), *size);
            }
            if component.kind == "postgres"
                && component.binding_kind.as_deref() == Some("container")
                && let Some(identity) = &component.binding_identity
                && let Ok(identity) = ExactContainerId::parse(identity.clone())
                && let Ok(Some(credentials)) = self
                    .inner
                    .files
                    .read_postgres_credentials(&component.deployment_id, &component.name)
                && let Ok(Some(facts)) = self.inner.docker.postgres_facts(
                    &identity,
                    &credentials.user,
                    &credentials.database,
                )
            {
                let key = SubjectKey::new(
                    "component",
                    format!("{}/{}", component.deployment_id, component.name),
                );
                storage
                    .entry(key.clone())
                    .or_insert_with(BTreeMap::new)
                    .extend(facts.clone());
                for (metric, value) in facts {
                    record(&mut working, &key.kind, &key.id, &metric, value as f64);
                }
            }
        }
        for (container_id, repository_id, deployment_id, component) in observed {
            let Some(size) = container_sizes.get(&container_id) else {
                continue;
            };
            if let Some(buckets) = per_repository.get_mut(&repository_id) {
                add(buckets, "container_layers", *size);
            }
            storage.insert(
                SubjectKey::new("component", format!("{deployment_id}/{component}")),
                BTreeMap::from([("container_layer".into(), *size)]),
            );
        }
        let mut shared_volumes = 0_u64;
        for (volume, size) in &shared.volumes {
            if let Some((repository_id, kind)) = volume_owners.get(volume)
                && let Some(buckets) = per_repository.get_mut(repository_id)
            {
                add(
                    buckets,
                    if kind == "postgres" {
                        "postgres_data"
                    } else {
                        "volumes"
                    },
                    *size,
                );
            } else {
                shared_volumes = shared_volumes.saturating_add(*size);
            }
        }
        for (repository_id, mut buckets) in per_repository {
            let total = buckets.values().copied().fold(0_u64, u64::saturating_add);
            buckets.insert("total".into(), total);
            record(
                &mut working,
                "repository",
                &repository_id,
                "storage_bytes",
                total as f64,
            );
            storage.insert(SubjectKey::new("repository", repository_id), buckets);
        }
        let state_size = self
            .inner
            .source
            .directory_size(&self.inner.config.state_dir, StdDuration::from_secs(60))
            .unwrap_or(0);
        let filesystem = self.inner.source.filesystem(Path::new("/"));
        record(
            &mut working,
            "host",
            "host",
            "storage_bytes",
            filesystem.used as f64,
        );
        let managed_total = storage
            .iter()
            .filter(|(key, _)| key.kind == "repository")
            .filter_map(|(_, value)| value.get("total"))
            .copied()
            .fold(0_u64, u64::saturating_add);
        let docker_shared = shared
            .images
            .saturating_add(shared.build_cache)
            .saturating_add(shared_volumes);
        storage.insert(
            SubjectKey::new("host", "storage"),
            BTreeMap::from([
                ("fs_used".into(), filesystem.used),
                ("managed_repositories".into(), managed_total),
                ("devcoordinator_state".into(), state_size),
                ("docker_shared".into(), docker_shared),
                ("docker_images".into(), shared.images),
                ("docker_build_cache".into(), shared.build_cache),
                ("docker_shared_volumes".into(), shared_volumes),
                (
                    "other".into(),
                    filesystem
                        .used
                        .saturating_sub(managed_total)
                        .saturating_sub(state_size)
                        .saturating_sub(docker_shared),
                ),
            ]),
        );
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(snapshot) = state.as_mut() {
            snapshot.storage = storage;
        } else {
            *state = Some(SamplerSnapshot {
                host: empty_host(),
                current: BTreeMap::new(),
                storage,
                meta: BTreeMap::new(),
            });
        }
        Ok(())
    }

    pub async fn serve(&self, mut shutdown: watch::Receiver<bool>) {
        let mut next_sample = tokio::time::Instant::now();
        let mut next_storage = tokio::time::Instant::now();
        loop {
            if *shutdown.borrow() {
                break;
            }
            let now = tokio::time::Instant::now();
            if now >= next_sample {
                let sampler = self.clone();
                let _ = tokio::task::spawn_blocking(move || sampler.tick()).await;
                next_sample =
                    tokio::time::Instant::now() + StdDuration::from_secs(u64::from(SAMPLE_SECONDS));
            }
            if now >= next_storage || self.inner.storage_requested.load(Ordering::SeqCst) {
                let sampler = self.clone();
                let _ = tokio::task::spawn_blocking(move || sampler.storage_tick()).await;
                next_storage = tokio::time::Instant::now()
                    + StdDuration::from_secs(u64::from(STORAGE_SECONDS));
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(StdDuration::from_millis(100)) => {}
            }
        }
        let _ = self.flush();
    }

    pub fn flush(&self) -> Result<(), ProtocolError> {
        let mut working = self
            .inner
            .working
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.flush_locked(&mut working)
    }

    fn flush_locked(&self, working: &mut Working) -> Result<(), ProtocolError> {
        let minute = working.minute.clone().unwrap_or_default();
        let aggregates = std::mem::take(&mut working.aggregates);
        self.inner.metrics.flush(&minute, &aggregates)
    }

    fn subjects(&self) -> Result<Vec<Subject>, ProtocolError> {
        let (worktrees, deployments, components) = self.runtime_records()?;
        let worktrees = worktrees
            .into_iter()
            .map(|worktree| (worktree.worktree_id, worktree.repository_id))
            .collect::<HashMap<_, _>>();
        let deployments = deployments
            .into_iter()
            .map(|deployment| (deployment.deployment_id.clone(), deployment))
            .collect::<HashMap<_, _>>();
        let mut subjects = Vec::new();
        let prefix = format!("{}-", self.inner.config.unit_prefix);
        for unit in self
            .inner
            .systemd
            .list_matching_units(&format!("{prefix}*.service"))
            .unwrap_or_default()
        {
            let worktree_id = unit
                .strip_prefix(&prefix)
                .and_then(|value| value.split('-').next());
            subjects.push(Subject {
                key: SubjectKey::new("test", &unit),
                meta: SubjectMeta {
                    repository_id: worktree_id.and_then(|id| worktrees.get(id)).cloned(),
                    ..Default::default()
                },
                cgroup: self.inner.systemd.control_group_path(&unit).ok().flatten(),
            });
        }
        for component in &components {
            let deployment = deployments.get(&component.deployment_id);
            let cgroup = match (
                component.binding_kind.as_deref(),
                component.binding_identity.as_deref(),
            ) {
                (Some("unit"), Some(unit)) => {
                    self.inner.systemd.control_group_path(unit).ok().flatten()
                }
                (Some("container"), Some(container)) => {
                    Some(self.inner.source.container_cgroup(container))
                }
                _ => None,
            };
            subjects.push(Subject {
                key: SubjectKey::new(
                    "component",
                    format!("{}/{}", component.deployment_id, component.name),
                ),
                meta: SubjectMeta {
                    repository_id: deployment.map(|deployment| deployment.repository_id.clone()),
                    deployment_id: Some(component.deployment_id.clone()),
                    component: Some(component.name.clone()),
                    component_type: Some(component.kind.clone()),
                    binding: component.binding_identity.clone(),
                    ..Default::default()
                },
                cgroup,
            });
        }
        if let Ok(inventory) = self.inner.inventory.containers() {
            for container in inventory.containers {
                subjects.push(Subject {
                    key: SubjectKey::new("container", &container.id),
                    meta: SubjectMeta {
                        repository_id: container.repository_id,
                        deployment_id: container.deployment_id,
                        component: container.component,
                        component_type: Some("container".into()),
                        name: Some(container.name),
                        image: Some(container.image),
                        state: Some(container.state.clone()),
                        ..Default::default()
                    },
                    cgroup: (container.state == "running")
                        .then(|| self.inner.source.container_cgroup(&container.id)),
                });
            }
        }
        Ok(subjects)
    }

    fn evaluate_alerts(
        &self,
        host: &HealthHost,
        snapshot: &SamplerSnapshot,
        working: &mut Working,
        now_mono: f64,
    ) -> Result<(), ProtocolError> {
        let mut conditions = vec![
            Condition {
                key: "host/cpu".into(),
                kind: "host_cpu".into(),
                subject_kind: "host".into(),
                subject_id: "host".into(),
                severity: "warning".into(),
                message: format!("host CPU {:.0}% sustained", host.cpu_percent),
                active: host.cpu_percent > 90.0,
                sustain_seconds: 300.0,
            },
            Condition {
                key: "host/memory".into(),
                kind: "host_memory".into(),
                subject_kind: "host".into(),
                subject_id: "host".into(),
                severity: "critical".into(),
                message: "host memory available below 10%".into(),
                active: host.memory_total > 0 && host.memory_available < host.memory_total / 10,
                sustain_seconds: 300.0,
            },
            Condition {
                key: "host/disk".into(),
                kind: "host_disk".into(),
                subject_kind: "host".into(),
                subject_id: "host".into(),
                severity: "critical".into(),
                message: "root filesystem below 10% free".into(),
                active: host.fs_size > 0 && host.fs_free < host.fs_size / 10,
                sustain_seconds: 0.0,
            },
        ];
        for component in self.component_records()? {
            let id = format!("{}/{}", component.deployment_id, component.name);
            conditions.push(Condition {
                key: format!("component/{id}/unhealthy"),
                kind: "component_unhealthy".into(),
                subject_kind: "component".into(),
                subject_id: id.clone(),
                severity: "critical".into(),
                message: format!("component {id} is {}", component.state),
                active: component.desired_state == "running" && component.state != "running",
                sustain_seconds: 120.0,
            });
            if component.binding_kind.as_deref() == Some("unit")
                && let Some(unit) = &component.binding_identity
            {
                let restarts = self
                    .inner
                    .systemd
                    .show_unit(unit, &["NRestarts"])
                    .ok()
                    .and_then(|values| {
                        values
                            .into_iter()
                            .find(|(name, _)| name == "NRestarts")
                            .and_then(|(_, value)| value.parse::<u32>().ok())
                    })
                    .unwrap_or(0);
                let history = working.restart_history.entry(id.clone()).or_default();
                history.push((now_mono, restarts));
                history.retain(|(at, _)| now_mono - *at <= 600.0);
                let delta = restarts.saturating_sub(history.first().map_or(restarts, |row| row.1));
                conditions.push(Condition {
                    key: format!("component/{id}/crashloop"),
                    kind: "crash_loop".into(),
                    subject_kind: "component".into(),
                    subject_id: id.clone(),
                    severity: "critical".into(),
                    message: format!("component {id} restarted {delta}x in 10 min"),
                    active: delta >= 3,
                    sustain_seconds: 0.0,
                });
            }
        }
        for (key, values) in &snapshot.storage {
            if key.kind == "worktree"
                && values.get("test_scratch").copied().unwrap_or(0) > 10 * 1024 * 1024 * 1024
            {
                conditions.push(Condition {
                    key: format!("worktree/{}/scratch", key.id),
                    kind: "test_scratch".into(),
                    subject_kind: "worktree".into(),
                    subject_id: key.id.clone(),
                    severity: "warning".into(),
                    message: "test scratch exceeds 10 GiB".into(),
                    active: true,
                    sustain_seconds: 0.0,
                });
            }
        }
        self.inner.alerts.evaluate(&conditions)
    }

    fn runtime_records(&self) -> Result<RuntimeRecords, ProtocolError> {
        self.inner
            .database
            .call(|connection| {
                let worktrees = query_worktrees(connection)?;
                let deployments = query_deployments(connection)?;
                let components = query_components(connection)?;
                Ok((worktrees, deployments, components))
            })
            .map_err(database_error)
    }

    #[allow(clippy::type_complexity)]
    fn storage_records(
        &self,
    ) -> Result<
        (
            Vec<(String, PathBuf)>,
            Vec<WorktreeRecord>,
            Vec<DeploymentRecord>,
            Vec<ComponentRecord>,
            Vec<(String, String, String, String)>,
        ),
        ProtocolError,
    > {
        self.inner
            .database
            .call(|connection| {
                let repositories = {
                    let mut statement = connection.prepare(
                        "SELECT repository_id,root_path FROM repositories WHERE archived_at IS NULL",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?)))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let observed = {
                    let mut statement = connection.prepare(
                        "SELECT container_id,repository_id,observed_deployment_id,compose_service FROM observed_containers",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                Ok((
                    repositories,
                    query_worktrees(connection)?,
                    query_deployments(connection)?,
                    query_components(connection)?,
                    observed,
                ))
            })
            .map_err(database_error)
    }

    fn component_records(&self) -> Result<Vec<ComponentRecord>, ProtocolError> {
        self.inner
            .database
            .call(|connection| query_components(connection))
            .map_err(database_error)
    }
}

fn query_worktrees(
    connection: &rusqlite::Connection,
) -> Result<Vec<WorktreeRecord>, DatabaseError> {
    let mut statement =
        connection.prepare("SELECT worktree_id,repository_id,worktree_path FROM worktrees")?;
    Ok(statement
        .query_map([], |row| {
            Ok(WorktreeRecord {
                worktree_id: row.get(0)?,
                repository_id: row.get(1)?,
                path: PathBuf::from(row.get::<_, String>(2)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn query_deployments(
    connection: &rusqlite::Connection,
) -> Result<Vec<DeploymentRecord>, DatabaseError> {
    let mut statement =
        connection.prepare("SELECT deployment_id,repository_id FROM deployments")?;
    Ok(statement
        .query_map([], |row| {
            Ok(DeploymentRecord {
                deployment_id: row.get(0)?,
                repository_id: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn query_components(
    connection: &rusqlite::Connection,
) -> Result<Vec<ComponentRecord>, DatabaseError> {
    let mut statement = connection.prepare(
        "SELECT deployment_id,name,type,desired_state,state,binding_kind,binding_identity FROM components",
    )?;
    Ok(statement
        .query_map([], |row| {
            Ok(ComponentRecord {
                deployment_id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                desired_state: row.get(3)?,
                state: row.get(4)?,
                binding_kind: row.get(5)?,
                binding_identity: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn delta_sample(
    working: &mut Working,
    key: &SubjectKey,
    stats: CgroupStats,
    now: f64,
    cpus: u32,
) -> MetricSample {
    let cpu = working.previous.get(key).map_or(0.0, |(previous, at)| {
        if now > *at {
            stats.cpu_usec.saturating_sub(*previous) as f64
                / ((now - *at) * 1_000_000.0 * f64::from(cpus.max(1)))
                * 100.0
        } else {
            0.0
        }
    });
    working.previous.insert(key.clone(), (stats.cpu_usec, now));
    MetricSample {
        cpu_percent: round(cpu.max(0.0), 3),
        cpu_usec_total: stats.cpu_usec,
        memory_bytes: stats.memory_current,
        memory_peak: stats.memory_peak,
        pids: stats.pids,
        io_read_bytes_total: stats.io_read_bytes,
        io_write_bytes_total: stats.io_write_bytes,
        io_read: 0,
        io_write: 0,
    }
}

fn record_sample(working: &mut Working, key: &SubjectKey, sample: &MetricSample) {
    for (metric, value) in [
        ("cpu_percent", sample.cpu_percent),
        ("cpu_usec_total", sample.cpu_usec_total as f64),
        ("memory_bytes", sample.memory_bytes as f64),
        ("memory_peak", sample.memory_peak as f64),
        ("pids", sample.pids as f64),
        ("io_read_bytes_total", sample.io_read_bytes_total as f64),
        ("io_write_bytes_total", sample.io_write_bytes_total as f64),
        ("io_read", sample.io_read as f64),
        ("io_write", sample.io_write as f64),
    ] {
        record(working, &key.kind, &key.id, metric, value);
    }
}

fn record(working: &mut Working, kind: &str, id: &str, metric: &str, value: f64) {
    if !value.is_finite() || metric.ends_with("_total") || metric == "memory_peak" {
        return;
    }
    let aggregate = working
        .aggregates
        .entry((kind.into(), id.into(), metric.into()))
        .or_insert(Aggregate {
            min: value,
            sum: 0.0,
            max: value,
            samples: 0,
        });
    aggregate.min = aggregate.min.min(value);
    aggregate.sum += value;
    aggregate.max = aggregate.max.max(value);
    aggregate.samples = aggregate.samples.saturating_add(1);
}

fn empty_metric() -> MetricSample {
    MetricSample {
        cpu_percent: 0.0,
        cpu_usec_total: 0,
        memory_bytes: 0,
        memory_peak: 0,
        pids: 0,
        io_read_bytes_total: 0,
        io_write_bytes_total: 0,
        io_read: 0,
        io_write: 0,
    }
}

fn empty_host() -> HealthHost {
    HealthHost {
        cpu_percent: 0.0,
        memory_total: 0,
        memory_used: 0,
        memory_available: 0,
        swap_total: 0,
        swap_used: 0,
        load_1: 0.0,
        load_5: 0.0,
        load_15: 0.0,
        fs_size: 0,
        fs_free: 0,
        fs_used: 0,
        ncpu: 1,
        reconciliation: HostReconciliation {
            managed_cpu_percent: 0.0,
            daemon_cpu_percent: 0.0,
            other_cpu_percent: 0.0,
            managed_memory: 0,
            daemon_memory: 0,
            other_memory: 0,
        },
    }
}

fn repository_storage() -> BTreeMap<String, u64> {
    [
        "checkout",
        "test_scratch",
        "deployment_artifacts",
        "container_layers",
        "volumes",
        "postgres_data",
    ]
    .into_iter()
    .map(|name| (name.into(), 0))
    .collect()
}

fn add(values: &mut BTreeMap<String, u64>, name: &str, value: u64) {
    let current = values.entry(name.into()).or_default();
    *current = current.saturating_add(value);
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10_f64.powi(places);
    (value * scale).round() / scale
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "metric sampler query failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{DockerError, DockerInvocation, DockerOutput, LogFollower};
    use crate::metrics_source::{DockerStorage, FilesystemStats, HostMemory};
    use crate::platform::FixedClock;
    use crate::systemd::{
        PersistentUnitSpec, ProcessState, SystemdError, TransientUnitSpec, UnitProcess,
    };
    use std::collections::HashSet;
    use std::sync::atomic::AtomicU64;
    use tempfile::tempdir;
    use time::macros::datetime;

    struct FakeDocker;
    impl DockerControl for FakeDocker {
        fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            let operation = invocation.args()[0].to_string_lossy();
            Ok(DockerOutput {
                exit_code: 0,
                stdout: if operation == "ps" {
                    String::new()
                } else {
                    "{}".into()
                },
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
        fn spawn_follow_logs(&self, _id: &ExactContainerId) -> Result<LogFollower, DockerError> {
            Err(DockerError::InvalidRequest("unexpected logs".into()))
        }
    }

    struct FakeSystemd;
    impl SystemdControl for FakeSystemd {
        fn spawn_transient(
            &self,
            _spec: &TransientUnitSpec,
        ) -> Result<Box<dyn UnitProcess>, SystemdError> {
            Err(SystemdError::Operation("unused".into()))
        }
        fn start_persistent(&self, _spec: &PersistentUnitSpec) -> Result<(), SystemdError> {
            Err(SystemdError::Operation("unused".into()))
        }
        fn process_state(&self, _unit: &str) -> Result<ProcessState, SystemdError> {
            Err(SystemdError::Operation("unused".into()))
        }
        fn show_unit(
            &self,
            _unit: &str,
            _properties: &[&str],
        ) -> Result<Vec<(String, String)>, SystemdError> {
            Ok(Vec::new())
        }
        fn list_matching_units(&self, _pattern: &str) -> Result<Vec<String>, SystemdError> {
            Ok(Vec::new())
        }
        fn stop_unit(&self, _unit: &str) -> Result<(), SystemdError> {
            Ok(())
        }
        fn reset_failed(&self, _unit: &str) -> Result<(), SystemdError> {
            Ok(())
        }
        fn control_group_path(&self, _unit: &str) -> Result<Option<PathBuf>, SystemdError> {
            Ok(None)
        }
        fn prove_cgroup_empty(&self, _cgroup: Option<&Path>, _deadline: StdDuration) -> bool {
            true
        }
        fn process_uids(&self, _pid: u32) -> Option<[u32; 4]> {
            None
        }
    }

    struct FakeSource {
        cpu: AtomicU64,
        request_during_scan: Mutex<Option<MetricSampler>>,
    }
    impl MetricSource for FakeSource {
        fn host_cpu_ticks(&self) -> (u64, u64) {
            let value = self.cpu.fetch_add(10, Ordering::SeqCst);
            (value, value * 2 + 100)
        }
        fn host_memory(&self) -> HostMemory {
            HostMemory {
                total: 1000,
                available: 400,
                used: 600,
                swap_total: 100,
                swap_free: 75,
            }
        }
        fn host_load(&self) -> (f64, f64, f64) {
            (1.0, 2.0, 3.0)
        }
        fn filesystem(&self, _: &Path) -> FilesystemStats {
            FilesystemStats {
                size: 1000,
                free: 500,
                used: 500,
            }
        }
        fn cgroup_stats(&self, _: Option<&Path>) -> Option<CgroupStats> {
            None
        }
        fn own_cgroup(&self) -> Option<PathBuf> {
            None
        }
        fn container_cgroup(&self, id: &str) -> PathBuf {
            PathBuf::from(format!("/cgroup/{id}"))
        }
        fn directory_size(&self, _: &Path, _: StdDuration) -> Option<u64> {
            if let Some(sampler) = self.request_during_scan.lock().unwrap().take() {
                sampler.request_storage();
            }
            Some(10)
        }
        fn container_sizes(&self) -> BTreeMap<String, u64> {
            BTreeMap::new()
        }
        fn docker_shared_sizes(&self) -> DockerStorage {
            DockerStorage::default()
        }
        fn logical_cpus(&self) -> u32 {
            2
        }
    }

    struct StepMonotonic(AtomicU64);
    impl MonotonicClock for StepMonotonic {
        fn seconds(&self) -> f64 {
            self.0.fetch_add(15, Ordering::SeqCst) as f64
        }
    }

    fn storage_fixture() -> (tempfile::TempDir, MetricSampler, Arc<FakeSource>) {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("run/daemon.sock"),
            state_dir: temporary.path().join("state"),
            unit_prefix: "fixture".into(),
            slice_name: "fixture.slice".into(),
            client_group: "clients".into(),
            port_range: (40000, 40100),
            base_domain: "example.test".into(),
            edge_uid: None,
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: temporary.path().join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        };
        std::fs::create_dir_all(&config.state_dir).unwrap();
        let docker = Arc::new(FakeDocker);
        let source = Arc::new(FakeSource {
            cpu: AtomicU64::new(10),
            request_during_scan: Mutex::new(None),
        });
        let sampler = MetricSampler::with_adapters(
            config,
            database,
            docker,
            Arc::new(FakeSystemd),
            source.clone(),
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
            Arc::new(StepMonotonic(AtomicU64::new(0))),
        );
        (temporary, sampler, source)
    }

    #[test]
    fn host_sampling_storage_and_minute_flush_are_consistent() {
        let (_temporary, sampler, _source) = storage_fixture();
        sampler.tick().unwrap();
        sampler.storage_tick().unwrap();
        let snapshot = sampler.snapshot().unwrap();
        assert_eq!(snapshot.host.memory_used, 600);
        assert_eq!(snapshot.host.load_15, 3.0);
        assert_eq!(
            snapshot.storage[&SubjectKey::new("host", "storage")]["fs_used"],
            500
        );
        sampler.flush().unwrap();
        assert!(sampler.metrics().table_size().unwrap() >= 3);
    }

    #[test]
    fn storage_refresh_requested_during_a_scan_is_not_lost() {
        let (_temporary, sampler, source) = storage_fixture();
        sampler.storage_tick().unwrap();
        assert!(!sampler.inner.storage_requested.load(Ordering::SeqCst));
        *source.request_during_scan.lock().unwrap() = Some(sampler.clone());
        sampler.storage_tick().unwrap();
        assert!(sampler.inner.storage_requested.load(Ordering::SeqCst));
        sampler.storage_tick().unwrap();
        assert!(!sampler.inner.storage_requested.load(Ordering::SeqCst));
    }
}
