//! Typed health, metric history, repository, and container projections.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use devcoordinator2_api::params::{
    HealthHistory as HealthHistoryParams, MetricName, MetricSubjectKind,
};
use devcoordinator2_api::results::{
    ContainerList, DeploymentListRow, HealthHistory, HealthRepositories, HealthRepository,
    HealthStorage, HealthSummary, MetricSample, RepositoryComponentMetric, RepositoryHealthRow,
    RepositoryStorage, ResourceSummary, SamplingSummary, SharedResourceSummary,
    UnhealthyDeployment, UnhealthyReason,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};

use crate::access::Caller;
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::deployment_state::DeploymentStore;
use crate::docker::{DockerCli, DockerControl};
use crate::metrics::{RETENTION_DAYS, downsample};
use crate::metrics_sampler::{MetricSampler, SubjectKey};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;
use crate::systemd::{SystemdCli, SystemdControl};

#[derive(Clone)]
pub struct HealthService {
    database: Database,
    registry: Registry,
    deployments: DeploymentStore,
    sampler: MetricSampler,
}

impl HealthService {
    pub fn new(config: Config, database: Database, registry: Registry) -> Self {
        Self::with_clock(config, database, registry, Arc::new(HostClock))
    }

    pub fn with_clock(
        config: Config,
        database: Database,
        registry: Registry,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::with_adapters(
            config,
            database,
            registry,
            Arc::new(DockerCli::default()),
            Arc::new(SystemdCli::default()),
            clock,
        )
    }

    pub fn with_adapters(
        config: Config,
        database: Database,
        registry: Registry,
        docker: Arc<dyn DockerControl>,
        systemd: Arc<dyn SystemdControl>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let sampler = MetricSampler::new(
            config,
            database.clone(),
            docker,
            systemd,
            Arc::clone(&clock),
        );
        Self::with_sampler(database, registry, sampler, clock)
    }

    pub fn with_sampler(
        database: Database,
        registry: Registry,
        sampler: MetricSampler,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            deployments: DeploymentStore::with_clock(database.clone(), clock),
            database,
            registry,
            sampler,
        }
    }

    pub fn sampler(&self) -> &MetricSampler {
        &self.sampler
    }

    pub fn summary(&self) -> Result<HealthSummary, ProtocolError> {
        let snapshot = self.sampler.snapshot()?;
        let active = self.active_repository_ids()?;
        let mut deployments = self.deployments.managed_list_rows(&active)?;
        deployments.extend(
            self.deployments
                .observed_list(None)?
                .into_iter()
                .filter(|deployment| active.contains(&deployment.repository_id)),
        );
        let mut unhealthy = Vec::new();
        for deployment in deployments.into_iter().filter(|deployment| {
            matches!(deployment.state.as_str(), "degraded" | "failed")
                || deployment.health.as_deref() == Some("unhealthy")
        }) {
            unhealthy.push(UnhealthyDeployment {
                reasons: self.unhealthy_reasons(&deployment)?,
                deployment,
            });
        }
        let inventory = self.sampler.inventory().containers();
        Ok(HealthSummary {
            host: snapshot.host,
            storage: host_storage(snapshot.storage.get(&SubjectKey::new("host", "storage"))),
            unhealthy_deployments: unhealthy,
            active_tests: snapshot
                .current
                .keys()
                .filter(|key| key.kind == "test")
                .map(|key| key.id.clone())
                .collect(),
            container_counts: inventory.map_or_else(|_| BTreeMap::new(), |value| value.counts),
            alerts: self.sampler.alerts().current()?,
            sampling: SamplingSummary {
                cpu_memory_seconds: super::metrics_sampler::SAMPLE_SECONDS,
                storage_seconds: super::metrics_sampler::STORAGE_SECONDS,
                aggregate: "1 minute".into(),
                retention_days: u32::try_from(RETENTION_DAYS).unwrap_or(30),
                stored_minutes: self.sampler.metrics().table_size()?,
            },
        })
    }

    pub fn repositories(&self) -> Result<HealthRepositories, ProtocolError> {
        let snapshot = self.sampler.snapshot()?;
        let repositories = self.registry.list_repositories(false)?.repositories;
        let active = repositories
            .iter()
            .map(|repository| repository.repository_id.clone())
            .collect::<HashSet<_>>();
        let managed = self.deployments.managed_list_rows(&active)?;
        let observed = self.deployments.observed_list(None)?;
        let mut rows = Vec::new();
        for repository in repositories {
            let key = SubjectKey::new("repository", &repository.repository_id);
            let live = snapshot
                .current
                .get(&key)
                .cloned()
                .unwrap_or_else(empty_metric);
            let storage = repository_storage(snapshot.storage.get(&key));
            let deployments = managed
                .iter()
                .chain(&observed)
                .filter(|deployment| deployment.repository_id == repository.repository_id)
                .cloned()
                .collect::<Vec<_>>();
            let health = if deployments.iter().any(|deployment| {
                matches!(deployment.state.as_str(), "degraded" | "failed")
                    || deployment.health.as_deref() == Some("unhealthy")
            }) {
                "unhealthy"
            } else if deployments.is_empty() {
                "none"
            } else {
                "healthy"
            };
            rows.push(RepositoryHealthRow {
                repository_id: repository.repository_id.clone(),
                display_name: repository.display_name,
                root_path: Some(repository.root_path),
                cpu_percent: live.cpu_percent,
                memory_bytes: live.memory_bytes,
                storage_bytes: Some(storage.total),
                storage,
                health: health.into(),
                deployments,
                trend_cpu: self.sampler.metrics().trend(
                    "repository",
                    &repository.repository_id,
                    "cpu_percent",
                    60,
                    12,
                )?,
                trend_memory: numeric_trend(self.sampler.metrics().trend(
                    "repository",
                    &repository.repository_id,
                    "memory_bytes",
                    60,
                    12,
                )?),
                trend_storage: numeric_trend(self.sampler.metrics().trend(
                    "repository",
                    &repository.repository_id,
                    "storage_bytes",
                    1_440,
                    12,
                )?),
            });
        }
        let daemon = snapshot
            .current
            .get(&SubjectKey::new("daemon", "daemon"))
            .cloned()
            .unwrap_or_else(empty_metric);
        let other = snapshot
            .current
            .get(&SubjectKey::new("other", "other"))
            .cloned()
            .unwrap_or_else(empty_metric);
        let host = host_storage(snapshot.storage.get(&SubjectKey::new("host", "storage")));
        Ok(HealthRepositories {
            repositories: rows,
            devcoordinator: ResourceSummary {
                cpu_percent: daemon.cpu_percent,
                memory_bytes: daemon.memory_bytes,
                storage_bytes: Some(host.devcoordinator_state),
            },
            shared_unattributed: SharedResourceSummary {
                cpu_percent: other.cpu_percent,
                memory_bytes: other.memory_bytes,
                storage: HealthStorage {
                    fs_used: 0,
                    managed_repositories: 0,
                    devcoordinator_state: 0,
                    docker_shared: host.docker_shared,
                    docker_images: host.docker_images,
                    docker_build_cache: host.docker_build_cache,
                    docker_shared_volumes: host.docker_shared_volumes,
                    other: host.other,
                },
            },
            host: snapshot.host,
        })
    }

    pub fn repository(
        &self,
        path: &str,
        caller: &Caller,
    ) -> Result<HealthRepository, ProtocolError> {
        if !Path::new(path).is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be absolute",
            ));
        }
        let status = self
            .registry
            .repository_status(Path::new(path), Some((caller.uid, caller.gid)))?;
        let snapshot = self.sampler.snapshot()?;
        let repository_key = SubjectKey::new("repository", &status.repository_id);
        let mut components = Vec::new();
        for (key, sample) in &snapshot.current {
            let Some(meta) = snapshot.meta.get(key) else {
                continue;
            };
            if meta.repository_id.as_deref() != Some(&status.repository_id) {
                continue;
            }
            components.push(RepositoryComponentMetric {
                kind: key.kind.clone(),
                id: key.id.clone(),
                deployment_id: meta.deployment_id.clone(),
                component: meta.component.clone(),
                component_type: meta.component_type.clone(),
                name: meta.name.clone(),
                image: meta.image.clone(),
                state: meta.state.clone(),
                binding: meta.binding.clone(),
                metric: sample.clone(),
                storage: repository_storage(snapshot.storage.get(key)),
            });
        }
        Ok(HealthRepository {
            repository_id: status.repository_id,
            display_name: status.display_name,
            live: snapshot.current.get(&repository_key).cloned(),
            storage: snapshot
                .storage
                .get(&repository_key)
                .map(|values| repository_storage(Some(values))),
            components,
        })
    }

    pub fn history(&self, params: HealthHistoryParams) -> Result<HealthHistory, ProtocolError> {
        let kind = subject_name(&params.subject_kind);
        let metric = metric_name(&params.metric);
        let mut points =
            self.sampler
                .metrics()
                .series(kind, &params.subject_id, metric, params.minutes)?;
        if let Some(target) = params.points {
            points = downsample(&points, usize::from(target));
        }
        let truncated = points.len() > 1_440;
        if truncated {
            points.drain(..points.len() - 1_440);
        }
        Ok(HealthHistory {
            subject_kind: params.subject_kind,
            subject_id: params.subject_id,
            metric: params.metric,
            minutes: params.minutes,
            points,
            truncated,
        })
    }

    pub fn containers(&self) -> Result<ContainerList, ProtocolError> {
        let snapshot = self.sampler.snapshot()?;
        let mut inventory = self.sampler.inventory().containers()?;
        for row in &mut inventory.containers {
            if let Some(sample) = snapshot.current.get(&SubjectKey::new("container", &row.id)) {
                row.cpu_percent = Some(sample.cpu_percent);
                row.memory_bytes = Some(sample.memory_bytes);
                row.pids = Some(sample.pids);
            }
            if let (Some(deployment), Some(component)) =
                (row.deployment_id.as_deref(), row.component.as_deref())
            {
                row.container_layer_bytes = snapshot
                    .storage
                    .get(&SubjectKey::new(
                        "component",
                        format!("{deployment}/{component}"),
                    ))
                    .and_then(|storage| storage.get("container_layer"))
                    .copied();
            }
        }
        Ok(inventory)
    }

    pub fn remove_container(
        &self,
        container_id: &str,
    ) -> Result<devcoordinator2_api::results::RemovedContainer, ProtocolError> {
        let result = self.sampler.inventory().remove(container_id)?;
        self.sampler.request_storage();
        Ok(result)
    }

    fn active_repository_ids(&self) -> Result<HashSet<String>, ProtocolError> {
        Ok(self
            .registry
            .list_repositories(false)?
            .repositories
            .into_iter()
            .map(|repository| repository.repository_id)
            .collect())
    }

    fn unhealthy_reasons(
        &self,
        deployment: &DeploymentListRow,
    ) -> Result<Vec<UnhealthyReason>, ProtocolError> {
        let deployment_id = deployment.deployment_id.clone();
        let observed = deployment.observed_only;
        self.database
            .call(move |connection| {
                let mut reasons = Vec::new();
                if observed {
                    let mut statement = connection.prepare(
                        "SELECT compose_service,state,status,health FROM observed_containers WHERE observed_deployment_id=?1 ORDER BY compose_service",
                    )?;
                    for row in statement.query_map([deployment_id], |row| {
                        Ok((
                            row.get::<_,String>(0)?,row.get::<_,String>(1)?,
                            row.get::<_,String>(2)?,row.get::<_,String>(3)?,
                        ))
                    })? {
                        let (component,state,status,health)=row?;
                        if state == "running" && health != "unhealthy" { continue; }
                        let detail = if health == "unhealthy" {
                            Some(if status != state { format!("container healthcheck failing ({status})") } else { "container healthcheck failing".into() })
                        } else if status != state { Some(status) } else { None };
                        reasons.push(UnhealthyReason { component,state,detail });
                    }
                } else {
                    let mut statement = connection.prepare(
                        "SELECT name,state,desired_state,health,last_error FROM components WHERE deployment_id=?1 ORDER BY order_index",
                    )?;
                    for row in statement.query_map([deployment_id], |row| {
                        Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?))
                    })? {
                        let (component,state,desired,health,detail)=row?;
                        if state == "running" && health != "unhealthy" { continue; }
                        if desired != "running" && state == "stopped" { continue; }
                        reasons.push(UnhealthyReason { component,state,detail });
                    }
                }
                Ok(reasons)
            })
            .map_err(database_error)
    }
}

fn host_storage(values: Option<&BTreeMap<String, u64>>) -> HealthStorage {
    let get = |name| {
        values
            .and_then(|values| values.get(name))
            .copied()
            .unwrap_or(0)
    };
    HealthStorage {
        fs_used: get("fs_used"),
        managed_repositories: get("managed_repositories"),
        devcoordinator_state: get("devcoordinator_state"),
        docker_shared: get("docker_shared"),
        docker_images: get("docker_images"),
        docker_build_cache: get("docker_build_cache"),
        docker_shared_volumes: get("docker_shared_volumes"),
        other: get("other"),
    }
}

fn repository_storage(values: Option<&BTreeMap<String, u64>>) -> RepositoryStorage {
    let get = |name| {
        values
            .and_then(|values| values.get(name))
            .copied()
            .unwrap_or(0)
    };
    RepositoryStorage {
        checkout: get("checkout"),
        test_scratch: get("test_scratch"),
        deployment_artifacts: get("deployment_artifacts"),
        container_layers: get("container_layers").max(get("container_layer")),
        volumes: get("volumes"),
        postgres_data: get("postgres_data"),
        total: get("total"),
        container_layer: values
            .and_then(|values| values.get("container_layer"))
            .copied(),
        pg_connections: values
            .and_then(|values| values.get("pg_connections"))
            .copied(),
        pg_wal_bytes: values
            .and_then(|values| values.get("pg_wal_bytes"))
            .copied(),
        pg_temp_bytes: values
            .and_then(|values| values.get("pg_temp_bytes"))
            .copied(),
        pg_database_bytes: values
            .and_then(|values| values.get("pg_database_bytes"))
            .copied(),
    }
}

fn numeric_trend(values: Vec<f64>) -> Vec<u64> {
    values
        .into_iter()
        .map(|value| value.max(0.0).round() as u64)
        .collect()
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

fn subject_name(kind: &MetricSubjectKind) -> &'static str {
    match kind {
        MetricSubjectKind::Host => "host",
        MetricSubjectKind::Repository => "repository",
        MetricSubjectKind::Component => "component",
        MetricSubjectKind::Container => "container",
        MetricSubjectKind::Test => "test",
        MetricSubjectKind::Daemon => "daemon",
        MetricSubjectKind::Other => "other",
        MetricSubjectKind::Worktree => "worktree",
        MetricSubjectKind::Deployment => "deployment",
    }
}

fn metric_name(metric: &MetricName) -> &'static str {
    match metric {
        MetricName::CpuPercent => "cpu_percent",
        MetricName::MemoryBytes => "memory_bytes",
        MetricName::Pids => "pids",
        MetricName::StorageBytes => "storage_bytes",
        MetricName::MemoryUsed => "memory_used",
        MetricName::Load1 => "load_1",
        MetricName::PgConnections => "pg_connections",
        MetricName::PgWalBytes => "pg_wal_bytes",
        MetricName::PgTempBytes => "pg_temp_bytes",
        MetricName::PgDatabaseBytes => "pg_database_bytes",
        MetricName::IoRead => "io_read",
        MetricName::IoWrite => "io_write",
    }
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "health query failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{
        DockerError, DockerInvocation, DockerOutput, ExactContainerId, LogFollower,
    };
    use crate::platform::FixedClock;
    use crate::systemd::{
        PersistentUnitSpec, ProcessState, SystemdError, TransientUnitSpec, UnitProcess,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::tempdir;
    use time::macros::datetime;

    struct FakeDocker;
    impl DockerControl for FakeDocker {
        fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            let args = invocation
                .args()
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>();
            Ok(DockerOutput {
                exit_code: 0,
                stdout: if args.first().is_some_and(|value| value == "system") {
                    "{}".into()
                } else {
                    String::new()
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
        fn prove_cgroup_empty(&self, _cgroup: Option<&Path>, _deadline: Duration) -> bool {
            true
        }
        fn process_uids(&self, _pid: u32) -> Option<[u32; 4]> {
            None
        }
    }

    #[test]
    fn storage_and_metric_names_are_total_and_stable() {
        let storage = repository_storage(Some(&BTreeMap::from([
            ("checkout".into(), 10),
            ("container_layer".into(), 5),
        ])));
        assert_eq!(storage.checkout, 10);
        assert_eq!(storage.container_layers, 5);
        assert_eq!(storage.container_layer, Some(5));
        let postgres = repository_storage(Some(&BTreeMap::from([
            ("pg_connections".into(), 3),
            ("pg_wal_bytes".into(), 7),
            ("pg_temp_bytes".into(), 11),
            ("pg_database_bytes".into(), 13),
        ])));
        assert_eq!(
            (
                postgres.pg_connections,
                postgres.pg_wal_bytes,
                postgres.pg_temp_bytes,
                postgres.pg_database_bytes,
            ),
            (Some(3), Some(7), Some(11), Some(13))
        );
        assert_eq!(subject_name(&MetricSubjectKind::Deployment), "deployment");
        assert_eq!(metric_name(&MetricName::PgWalBytes), "pg_wal_bytes");
    }

    #[test]
    fn health_surfaces_share_one_sample_and_explain_unhealthy_components() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repository)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let registry = Registry::new(database.clone());
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Other,
            client_session: None,
            work: None,
            identity: None,
        };
        let registered = registry
            .register(&repository, caller.uid, caller.gid)
            .unwrap();
        database.transaction({let registered=registered.clone();move |transaction| {
            transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1111111111111111',?1,?2,'web','worktree',NULL,'f','{}','failed','t',1,'other','t')",rusqlite::params![registered.repository_id,registered.worktree_id])?;
            transaction.execute("INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,restarts,last_error,updated_at) VALUES('d1111111111111111','api','process',0,'f','running','failed','unhealthy',0,'boom','t')",[])?;
            Ok(())
        }}).unwrap();
        let config = Config {
            socket_path: temporary.path().join("run/daemon.sock"),
            state_dir: state,
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
        let health = HealthService::with_adapters(
            config,
            database,
            registry,
            Arc::new(FakeDocker),
            Arc::new(FakeSystemd),
            Arc::new(FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        health.sampler().tick().unwrap();
        health.sampler().storage_tick().unwrap();
        health.sampler().flush().unwrap();
        let summary = health.summary().unwrap();
        assert_eq!(summary.unhealthy_deployments.len(), 1);
        assert_eq!(
            summary.unhealthy_deployments[0].reasons[0]
                .detail
                .as_deref(),
            Some("boom")
        );
        assert_eq!(summary.container_counts["unmanaged"], 0);
        let repositories = health.repositories().unwrap();
        assert_eq!(repositories.repositories[0].health, "unhealthy");
        let detail = health
            .repository(repository.to_str().unwrap(), &caller)
            .unwrap();
        assert_eq!(detail.repository_id, registered.repository_id);
        let history = health
            .history(HealthHistoryParams {
                subject_kind: MetricSubjectKind::Host,
                subject_id: "host".into(),
                metric: MetricName::MemoryUsed,
                minutes: 60,
                points: None,
            })
            .unwrap();
        assert!(!history.points.is_empty());
        assert!(health.containers().unwrap().containers.is_empty());
        assert_eq!(
            health.remove_container("short").unwrap_err().code,
            ErrorCode::ParamsInvalid
        );
    }
}
