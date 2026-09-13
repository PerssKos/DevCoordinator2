//! Deployment resolution, truthful status, observed control, and route changes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::params::{DeploymentList as DeploymentListParams, SetDomain};
use devcoordinator2_api::results::{
    Component, ComponentBinding, ComposeService, DeclaredDeployment, DeploymentList, DeploymentLog,
    DeploymentSource, DeploymentStatus, DomainChanged,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use time::{format_description::FormatItem, macros::format_description};

use crate::access::Caller;
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::deployment_files::{DeploymentFiles, EnvironmentFormat, PostgresCredentials};
use crate::deployment_git::{DeploymentGit, GitCli};
use crate::deployment_health::{DeploymentHealth, HostDeploymentHealth, Readiness};
use crate::deployment_state::{
    ComponentRow, ComponentRuntimePatch, ComposeCompletionInput, DeploymentRow,
    DeploymentRuntimePatch, DeploymentStore, RegisteredDeploymentTarget,
};
use crate::docker::{
    ComposeContext, CreateContainerRequest, DockerCli, DockerControl, ExactContainerId,
    ManagedLabelContext, RuntimeState, compose_project, container_name, managed_volume_name,
};
use crate::platform::{Clock, HostClock};
use crate::repository::Registry;
use crate::repository_config::{
    ComponentKind, ComponentSpec, DeploymentSpec, list_deployment_names, load_deployment_spec,
};
use crate::routes::RouteFilePublisher;
use crate::systemd::{
    PersistentUnitSpec, SystemdCli, SystemdControl, TransientUnitSpec, primary_gid,
};

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
const BUILD_TIMEOUT: u64 = 1_800;

pub trait NetworkProbe: Send + Sync + 'static {
    fn tcp(&self, host: &str, port: u16) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostNetworkProbe;

impl NetworkProbe for HostNetworkProbe {
    fn tcp(&self, host: &str, port: u16) -> bool {
        (host, port)
            .to_socket_addrs()
            .ok()
            .into_iter()
            .flatten()
            .any(|address| TcpStream::connect_timeout(&address, Duration::from_secs(3)).is_ok())
    }
}

#[derive(Clone)]
pub struct Deployments {
    config: Arc<Config>,
    configuration: crate::runtime_configuration::RuntimeConfiguration,
    database: Database,
    registry: Registry,
    store: DeploymentStore,
    docker: Arc<dyn DockerControl>,
    systemd: Arc<dyn SystemdControl>,
    network: Arc<dyn NetworkProbe>,
    git: Arc<dyn DeploymentGit>,
    health: Arc<dyn DeploymentHealth>,
    files: DeploymentFiles,
    clock: Arc<dyn Clock>,
    port_availability: Arc<dyn crate::ports::PortAvailability>,
    busy: Arc<Mutex<HashSet<String>>>,
    finite_operations: crate::deployment_cancellation::FiniteOperations,
    routes: RouteFilePublisher,
}

struct ResolvedDeployment {
    row: DeploymentRow,
    specification: DeploymentSpec,
}

struct DeploymentTarget {
    row: Option<DeploymentRow>,
    deployment_id: String,
    repository_id: String,
    worktree_id: String,
    worktree: PathBuf,
    specification: DeploymentSpec,
    source: String,
    caller_uid: u32,
    caller_gid: u32,
    client: String,
    session: Option<String>,
}

fn deployment_fingerprint(
    target: &DeploymentTarget,
    snapshot: &crate::deployment_git::GitSnapshot,
) -> String {
    DeploymentStore::fingerprint(&serde_json::json!({
        "spec": target.specification.canonical(&target.source),
        "commit": snapshot.commit,
        "dirty": snapshot.dirty,
        "source_digest": snapshot.source_digest,
    }))
}

#[derive(Clone)]
struct StartedBinding {
    specification: ComponentSpec,
    kind: String,
    identity: String,
}

type DesiredSnapshot = (
    BTreeMap<String, String>,
    BTreeMap<String, BTreeMap<String, String>>,
);

struct BusyGuard {
    deployment_id: String,
    active: Arc<Mutex<HashSet<String>>>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.deployment_id);
    }
}

impl Deployments {
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
            Arc::new(HostNetworkProbe),
            clock,
        )
    }

    pub fn with_adapters(
        config: Config,
        database: Database,
        registry: Registry,
        docker: Arc<dyn DockerControl>,
        systemd: Arc<dyn SystemdControl>,
        network: Arc<dyn NetworkProbe>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let files = DeploymentFiles::new(config.deployments_dir(), config.secrets_dir());
        Self::with_runtime_adapters(
            config,
            database,
            registry,
            docker,
            systemd,
            network,
            Arc::new(GitCli),
            Arc::new(HostDeploymentHealth::default()),
            files,
            Arc::new(crate::ports::HostPortAvailability),
            clock,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_runtime_adapters(
        config: Config,
        database: Database,
        registry: Registry,
        docker: Arc<dyn DockerControl>,
        systemd: Arc<dyn SystemdControl>,
        network: Arc<dyn NetworkProbe>,
        git: Arc<dyn DeploymentGit>,
        health: Arc<dyn DeploymentHealth>,
        files: DeploymentFiles,
        port_availability: Arc<dyn crate::ports::PortAvailability>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let routes = RouteFilePublisher::new(
            database.clone(),
            config.routes_path(),
            config.base_domain.clone(),
        );
        Self {
            configuration: crate::runtime_configuration::RuntimeConfiguration::new(
                &config,
                database.clone(),
            ),
            config: Arc::new(config),
            store: DeploymentStore::with_clock(database.clone(), Arc::clone(&clock)),
            database,
            registry,
            docker,
            systemd,
            network,
            git,
            health,
            files,
            clock,
            port_availability,
            busy: Arc::new(Mutex::new(HashSet::new())),
            finite_operations: crate::deployment_cancellation::FiniteOperations::default(),
            routes,
        }
    }

    pub fn store(&self) -> &DeploymentStore {
        &self.store
    }

    pub fn configuration(&self) -> &crate::runtime_configuration::RuntimeConfiguration {
        &self.configuration
    }

    pub fn set_compose_authorization(
        &self,
        params: devcoordinator2_api::params::SetComposeEnvAuthorization,
        caller: &Caller,
    ) -> Result<devcoordinator2_api::configuration::Snapshot, ProtocolError> {
        let target = self.resolve_target_readonly(
            params.path.as_deref(),
            params.name.as_deref(),
            params.deployment_id.as_deref(),
            caller,
        )?;
        if params.authorized {
            if target.source != "worktree"
                || !target.specification.components.iter().any(|component| {
                    component.compose_env_file.as_deref() == Some(params.file.as_str())
                })
            {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "the environment file must be declared by this worktree deployment",
                ));
            }
            self.validate_compose_environment(&target, &params.file)?;
        }
        let actor = caller
            .identity
            .clone()
            .unwrap_or_else(|| format!("uid:{}", caller.uid));
        self.configuration.update(
            &target.repository_id,
            &params.file,
            params.authorized,
            &params.expected_revision,
            &actor,
        )
    }

    pub fn preflight(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<devcoordinator2_api::results::DeploymentPreflight, ProtocolError> {
        let target = self.resolve_target_readonly(path, name, deployment_id, caller)?;
        Ok(self.prerequisites(&target))
    }

    fn prerequisites(
        &self,
        target: &DeploymentTarget,
    ) -> devcoordinator2_api::results::DeploymentPreflight {
        let mut blockers = Vec::new();
        for component in &target.specification.components {
            let Some(file) = component.compose_env_file.as_deref() else {
                continue;
            };
            let error = if !self.configuration.authorized(&target.repository_id, file) {
                Some(ProtocolError::new(
                    ErrorCode::AuthorizationRequired,
                    "an administrator must authorize this exact repository environment file using config authorize",
                ))
            } else {
                self.validate_compose_environment(target, file).err()
            };
            if let Some(error) = error {
                blockers.push(devcoordinator2_api::results::DeploymentBlocker {
                    component: component.name.clone(),
                    code: error.code,
                    file: Some(file.to_owned()),
                    message: error.message,
                });
            }
        }
        devcoordinator2_api::results::DeploymentPreflight {
            repository_id: target.repository_id.clone(),
            name: target.specification.name.clone(),
            ready: blockers.is_empty(),
            blockers,
        }
    }

    fn validate_compose_environment(
        &self,
        target: &DeploymentTarget,
        file: &str,
    ) -> Result<(), ProtocolError> {
        self.files
            .validate_repository_file(&target.worktree, file)
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::RepositoryConfigInvalid,
                    "declared environment file is unavailable or unsafe",
                )
            })?;
        if !self
            .git
            .is_ignored(&target.worktree, file, target.caller_uid, target.caller_gid)
            .map_err(git_apply_error)?
        {
            return Err(ProtocolError::new(
                ErrorCode::RepositoryConfigInvalid,
                "declared environment file must remain Git-ignored",
            ));
        }
        Ok(())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn list(
        &self,
        params: DeploymentListParams,
        caller: &Caller,
    ) -> Result<DeploymentList, ProtocolError> {
        let repositories = self.registry.list_repositories(false)?;
        let active = repositories
            .repositories
            .iter()
            .map(|repository| repository.repository_id.clone())
            .collect::<HashSet<_>>();
        let mut deployments = self.store.managed_list_rows(&active)?;
        deployments.extend(
            self.store
                .observed_list(None)?
                .into_iter()
                .filter(|row| active.contains(&row.repository_id)),
        );
        let mut declared = Vec::new();
        if let Some(path) = params.path {
            if caller.identity.is_some() {
                return Err(ProtocolError::new(
                    ErrorCode::PermissionDenied,
                    "public callers cannot discover deployments from a path",
                ));
            }
            let registered = self
                .registry
                .register(Path::new(&path), caller.uid, caller.gid)?;
            let worktree = PathBuf::from(&registered.worktree_path);
            for name in list_deployment_names(&worktree).map_err(config_error)? {
                let specification = load_deployment_spec(&worktree, &name).map_err(config_error)?;
                for source in specification.sources {
                    declared.push(DeclaredDeployment {
                        deployment_id: DeploymentStore::deployment_id(
                            &registered.worktree_id,
                            &name,
                            &source,
                        ),
                        name: name.clone(),
                        source: deployment_source(&source)?,
                    });
                }
            }
        }
        Ok(DeploymentList {
            deployments,
            declared,
        })
    }

    pub fn status(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentStatus, ProtocolError> {
        if let Some(deployment_id) = deployment_id
            && self.store.observed_exists(deployment_id)?
        {
            return self
                .store
                .observed_status(deployment_id)?
                .ok_or_else(|| not_found(deployment_id));
        }
        let resolved = self.resolve(path, name, deployment_id, caller)?;
        let mut status = self.managed_status(&resolved)?;
        match self.resolve_target_readonly(path, name, deployment_id, caller) {
            Ok(target) => self.refresh_readiness(&target, &mut status)?,
            Err(error) => {
                if let Some(readiness) = &mut status.readiness {
                    readiness
                        .blockers
                        .push(devcoordinator2_api::results::DeploymentBlocker {
                        component: String::new(),
                        code: error.code,
                        file: None,
                        message:
                            "current deployment source is unavailable; readiness cannot be verified"
                                .into(),
                    });
                }
            }
        }
        Ok(status)
    }

    fn refresh_readiness(
        &self,
        target: &DeploymentTarget,
        status: &mut DeploymentStatus,
    ) -> Result<(), ProtocolError> {
        let Some(readiness) = &mut status.readiness else {
            return Ok(());
        };
        readiness
            .blockers
            .extend(self.prerequisites(target).blockers);
        match self
            .git
            .snapshot(&target.worktree, target.caller_uid, target.caller_gid)
        {
            Ok(snapshot) => {
                let fingerprint = deployment_fingerprint(target, &snapshot);
                let applied = self.store.get(&target.deployment_id)?;
                readiness.pending_apply = applied.map(|row| row.spec_fingerprint != fingerprint);
            }
            Err(_) => readiness
                .blockers
                .push(devcoordinator2_api::results::DeploymentBlocker {
                    component: String::new(),
                    code: ErrorCode::DeploymentApplyFailed,
                    file: None,
                    message: "current source could not be compared with the applied generation"
                        .into(),
                }),
        }
        readiness.ready = matches!(status.state.as_str(), "running" | "completed")
            && readiness.missing_components.is_empty()
            && readiness.pending_apply == Some(false)
            && readiness.blockers.is_empty();
        Ok(())
    }

    pub fn set_domain(&self, params: SetDomain) -> Result<DomainChanged, ProtocolError> {
        if let Some(domain) = params.domain.as_deref()
            && let Some(owner) = self.store.domain_owner(domain)?
            && owner != params.deployment_id
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("domain {domain:?} is already routed to {owner}"),
            ));
        }
        let mut result = if self.store.observed_exists(&params.deployment_id)? {
            self.store.set_observed_domain(
                &params.deployment_id,
                params.domain.as_deref(),
                params.port,
                params.component.as_deref(),
                params.public,
            )?
        } else {
            if params.port.is_some() || params.component.is_some() {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "port and component apply only to observed deployments",
                ));
            }
            let mut changed = self
                .store
                .override_managed_domain(&params.deployment_id, params.domain.as_deref())?;
            if let Some(public) = params.public {
                self.store.set_public(&params.deployment_id, public)?;
                changed.public = Some(public);
            }
            changed
        };
        self.routes.publish_current()?;
        result.observed_only.get_or_insert(false);
        Ok(result)
    }

    pub fn apply(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentStatus, ProtocolError> {
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        if target.caller_uid == 0 {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                "repository code never runs as root; call as a non-root account",
            ));
        }
        let _busy = self.acquire_busy(&target.deployment_id)?;
        let snapshot = self
            .git
            .snapshot(&target.worktree, target.caller_uid, target.caller_gid)
            .map_err(git_apply_error)?;
        let prerequisites = self.prerequisites(&target);
        if !prerequisites.ready {
            let code = prerequisites.blockers[0].code;
            return Err(ProtocolError::new(code, "deployment prerequisites are not satisfied; no runtime resources were changed")
                .with_detail(serde_json::json!({"blockers":prerequisites.blockers.iter().take(3).collect::<Vec<_>>(),"additional_blockers":prerequisites.blockers.len().saturating_sub(3),"inspect":"deployment preflight"}).to_string()));
        }
        let fingerprint = deployment_fingerprint(&target, &snapshot);
        let domain = DeploymentStore::effective_domain(
            target.row.as_ref(),
            &target.specification,
            &target.source,
        );
        if let Some(domain) = &domain
            && let Some(owner) = self.store.domain_owner(domain)?
            && owner != target.deployment_id
        {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                format!("domain {domain:?} is assigned to {owner}"),
            ));
        }
        if let Some(row) = &target.row
            && row.spec_fingerprint == fingerprint
            && matches!(row.state.as_str(), "running" | "completed")
            && !snapshot.dirty
        {
            let resolved = ResolvedDeployment {
                row: row.clone(),
                specification: target.specification.clone(),
            };
            let mut status = self.managed_status(&resolved)?;
            let route_expected =
                domain.is_some() && target.specification.route_component().is_some();
            let route_healthy = self.validate_or_withdraw_route(&target.deployment_id)?;
            if matches!(status.state.as_str(), "running" | "completed")
                && (!route_expected || route_healthy)
            {
                status.unchanged = Some(true);
                self.refresh_readiness(&target, &mut status)?;
                return Ok(status);
            }
        }

        let generation = target
            .row
            .as_ref()
            .and_then(|row| row.current_generation)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "deployment generation counter is exhausted",
                )
            })?;
        let ttl_expires_at = target
            .specification
            .ttl_seconds
            .map(|seconds| {
                let seconds = i64::try_from(seconds).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::RepositoryConfigInvalid,
                        "deployment TTL exceeds the supported range",
                    )
                })?;
                (self.clock.now_utc() + time::Duration::seconds(seconds))
                    .format(TIMESTAMP_FORMAT)
                    .map_err(|error| {
                        ProtocolError::new(
                            ErrorCode::InternalError,
                            "cannot format deployment expiry",
                        )
                        .with_detail(error.to_string())
                    })
            })
            .transpose()?;
        let old_components = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .map(|component| (component.name.clone(), component))
            .collect::<BTreeMap<_, _>>();
        let desired = self.snapshot_desired(&target.deployment_id, &old_components)?;
        self.store.upsert_with_fingerprint(
            &target.deployment_id,
            &RegisteredDeploymentTarget {
                repository_id: target.repository_id.clone(),
                worktree_id: target.worktree_id.clone(),
            },
            &target.specification,
            &target.source,
            &fingerprint,
            domain.as_deref(),
            "applying",
            target.caller_uid,
            &target.client,
            ttl_expires_at.as_deref(),
        )?;
        let _finite_guard = target
            .specification
            .components
            .iter()
            .all(ComponentSpec::is_finite_workload)
            .then(|| self.finite_operations.begin(&target.deployment_id));
        let generation_path =
            match self.prepare_generation(&target, generation, snapshot.commit.as_deref()) {
                Ok(path) => path,
                Err(error) => {
                    self.restore_after_preparation_failure(&target, &old_components, &desired)?;
                    return Err(error);
                }
            };
        self.store.add_generation(
            &target.deployment_id,
            generation,
            snapshot.commit.as_deref(),
            snapshot.dirty,
            &generation_path,
            &fingerprint,
        )?;
        if let Err(error) = self.run_build(&target, generation, &generation_path) {
            self.remove_generation_path(&target, generation, &generation_path);
            self.store
                .set_generation_state(&target.deployment_id, generation, "failed")?;
            self.restore_after_preparation_failure(&target, &old_components, &desired)?;
            return Err(error);
        }
        let result = self.converge(
            &target,
            &old_components,
            generation,
            &generation_path,
            domain.as_deref(),
            None,
            desired,
        );
        let mut status = result?;
        self.refresh_readiness(&target, &mut status)?;
        Ok(status)
    }

    pub fn rollback(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentStatus, ProtocolError> {
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        let row = target
            .row
            .as_ref()
            .ok_or_else(|| not_found(&target.deployment_id))?;
        let _busy = self.acquire_busy(&target.deployment_id)?;
        if target.source != "checkout" {
            return Err(ProtocolError::new(
                ErrorCode::RollbackUnavailable,
                "worktree deployments keep no previous generation",
            ));
        }
        let previous = row.previous_generation.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::RollbackUnavailable,
                "no previous generation retained",
            )
        })?;
        let retained = self
            .store
            .generation(&target.deployment_id, previous)?
            .filter(|generation| generation.path.is_dir())
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RollbackUnavailable,
                    "no previous generation retained",
                )
            })?;
        let current = row.current_generation.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::RollbackUnavailable,
                "deployment has no current generation",
            )
        })?;
        let generation = current.checked_add(1).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::RollbackUnavailable,
                "deployment generation counter is exhausted",
            )
        })?;
        let old_components = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .map(|component| (component.name.clone(), component))
            .collect::<BTreeMap<_, _>>();
        let desired = self.snapshot_desired(&target.deployment_id, &old_components)?;
        self.store.patch_deployment_runtime(
            &target.deployment_id,
            DeploymentRuntimePatch {
                state: Some("applying".into()),
                spec_fingerprint: Some(retained.fingerprint.clone()),
                ..Default::default()
            },
        )?;
        self.store.add_generation(
            &target.deployment_id,
            generation,
            retained.commit_hash.as_deref(),
            retained.dirty,
            &retained.path,
            &retained.fingerprint,
        )?;
        self.converge(
            &target,
            &old_components,
            generation,
            &retained.path,
            DeploymentStore::effective_domain(
                target.row.as_ref(),
                &target.specification,
                &target.source,
            )
            .as_deref(),
            Some((current, previous)),
            desired,
        )
    }

    pub fn control(
        &self,
        action: &str,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        component: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentStatus, ProtocolError> {
        if !matches!(action, "start" | "stop" | "restart") {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "deployment action is invalid",
            ));
        }
        if let Some(deployment_id) = deployment_id
            && self.store.observed_exists(deployment_id)?
        {
            return self.control_observed(action, deployment_id, component);
        }
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        let row = target
            .row
            .clone()
            .ok_or_else(|| not_found(&target.deployment_id))?;
        if action == "stop"
            && component.is_none()
            && let Some((requested, finished)) = self
                .finite_operations
                .cancel_and_wait(&target.deployment_id, Duration::from_secs(15))
        {
            let current = self
                .store
                .get(&target.deployment_id)?
                .ok_or_else(|| not_found(&target.deployment_id))?;
            let mut status = self.managed_status(&ResolvedDeployment {
                row: current,
                specification: target.specification,
            })?;
            if requested && !finished {
                status.state = "stopping".into();
            }
            return Ok(status);
        }
        let _busy = self.acquire_busy(&target.deployment_id)?;
        if let Some(reference) = component
            && let Some((parent, service)) = reference.split_once('/')
        {
            return self.control_compose_service(action, &target, parent, service);
        }
        let mut components = target.specification.components.clone();
        if let Some(component) = component {
            let specification = target.specification.component(component).ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!("no component {component:?}"),
                )
            })?;
            if !specification.independent_control {
                return Err(ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    format!("component {component:?} declares independent_control = false"),
                ));
            }
            components = vec![specification.clone()];
        }
        if matches!(action, "stop" | "restart") {
            let mut reverse = components.clone();
            reverse.reverse();
            self.stop_components(&target, &row, &reverse)?;
        }
        if matches!(action, "start" | "restart") {
            self.start_components(&target, &row, &components)?;
        }
        self.recompute_state(&target)?;
        let row = self
            .store
            .get(&target.deployment_id)?
            .ok_or_else(|| not_found(&target.deployment_id))?;
        self.managed_status(&ResolvedDeployment {
            row,
            specification: target.specification,
        })
    }

    pub fn logs(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        component: &str,
        tail_lines: u16,
        caller: &Caller,
    ) -> Result<DeploymentLog, ProtocolError> {
        if let Some(deployment_id) = deployment_id
            && self.store.observed_exists(deployment_id)?
        {
            return self.observed_logs(deployment_id, component, tail_lines);
        }
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        let row = target
            .row
            .as_ref()
            .ok_or_else(|| not_found(&target.deployment_id))?;
        if component == "build" {
            let (tail, truncated) = self
                .files
                .read_log_tail(&target.deployment_id, component, tail_lines)
                .map_err(file_action_error)?;
            return Ok(DeploymentLog {
                deployment_id: Some(target.deployment_id),
                component: component.into(),
                tail,
                truncated_before_tail: Some(truncated),
                log_path: None,
                container_id: None,
                observed_only: Some(false),
            });
        }
        let specification = target.specification.component(component).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("no component {component:?}"),
            )
        })?;
        let stored = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .find(|stored| stored.name == component)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!("no component {component:?}"),
                )
            })?;
        match (
            specification.kind,
            stored.binding_kind.as_deref(),
            stored.binding_identity.as_deref(),
        ) {
            (ComponentKind::Process, _, _) => {
                let (tail, truncated) = self
                    .files
                    .read_log_tail(&target.deployment_id, component, tail_lines)
                    .map_err(file_action_error)?;
                Ok(DeploymentLog {
                    deployment_id: Some(target.deployment_id),
                    component: component.into(),
                    tail,
                    truncated_before_tail: Some(truncated),
                    log_path: None,
                    container_id: None,
                    observed_only: Some(false),
                })
            }
            (_, Some("container"), Some(identity)) => {
                let identity = ExactContainerId::parse(identity.to_owned()).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        "recorded container identity is invalid",
                    )
                })?;
                Ok(DeploymentLog {
                    deployment_id: Some(target.deployment_id),
                    component: component.into(),
                    tail: self
                        .docker
                        .container_logs(&identity, tail_lines)
                        .map_err(runtime_error)?,
                    truncated_before_tail: Some(true),
                    log_path: None,
                    container_id: Some(identity.to_string()),
                    observed_only: Some(false),
                })
            }
            (_, Some("compose"), Some(project)) => {
                let generation = self.runtime_generation(row, &stored)?;
                let path = self
                    .store
                    .generation(&target.deployment_id, generation)?
                    .map_or_else(|| target.worktree.clone(), |generation| generation.path);
                let mut context =
                    self.compose_context(&target, specification, &path, generation)?;
                context.project = project.to_owned();
                Ok(DeploymentLog {
                    deployment_id: Some(target.deployment_id),
                    component: component.into(),
                    tail: self
                        .docker
                        .compose_logs(&context, tail_lines)
                        .map_err(runtime_error)?,
                    truncated_before_tail: Some(true),
                    log_path: None,
                    container_id: None,
                    observed_only: Some(false),
                })
            }
            _ => Ok(DeploymentLog {
                deployment_id: Some(target.deployment_id),
                component: component.into(),
                tail: String::new(),
                truncated_before_tail: Some(false),
                log_path: None,
                container_id: None,
                observed_only: Some(false),
            }),
        }
    }

    pub fn remove(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        delete_data: bool,
        caller: &Caller,
    ) -> Result<devcoordinator2_api::results::DeploymentRemoved, ProtocolError> {
        if let Some(deployment_id) = deployment_id {
            self.reject_observed_configuration(deployment_id)?;
        }
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        let row = target
            .row
            .clone()
            .ok_or_else(|| not_found(&target.deployment_id))?;
        let _busy = self.acquire_busy(&target.deployment_id)?;
        let mut components = target.specification.components.clone();
        components.reverse();
        self.stop_components(&target, &row, &components)?;
        let stored = self.store.components(&target.deployment_id)?;
        let mut deleted_volumes = Vec::new();
        for component in stored {
            let specification = target.specification.component(&component.name);
            if component.binding_kind.as_deref() == Some("container")
                && let Some(identity) = component.binding_identity
            {
                let identity = ExactContainerId::parse(identity).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        "recorded container identity is invalid",
                    )
                })?;
                self.docker
                    .remove_container(&identity, false)
                    .map_err(runtime_error)?;
                if delete_data && let Some(specification) = specification {
                    let names = if specification.kind == ComponentKind::Postgres {
                        vec!["pgdata"]
                    } else {
                        specification
                            .volumes
                            .iter()
                            .filter_map(|volume| volume.split_once(':').map(|value| value.0))
                            .collect()
                    };
                    for name in names {
                        let volume =
                            managed_volume_name(&target.deployment_id, &component.name, name);
                        self.docker.remove_volume(&volume).map_err(runtime_error)?;
                        deleted_volumes.push(volume.to_string());
                    }
                }
            } else if let Some(specification) = specification
                && specification.kind == ComponentKind::Compose
            {
                let generation = self.runtime_generation(&row, &component)?;
                let path = self
                    .store
                    .generation(&target.deployment_id, generation)?
                    .map_or_else(|| target.worktree.clone(), |generation| generation.path);
                let context = self.compose_context(&target, specification, &path, generation)?;
                self.docker
                    .compose_down(&context, delete_data)
                    .map_err(runtime_error)?;
            }
        }
        for generation in self.store.generations(&target.deployment_id)? {
            self.remove_recorded_generation_path(&target, &generation.path);
        }
        self.store.delete_rows(&target.deployment_id)?;
        self.routes.publish_current()?;
        if delete_data {
            self.files
                .delete_secrets(&target.deployment_id)
                .map_err(file_action_error)?;
        }
        self.files
            .cleanup_runtime_files(&target.deployment_id)
            .map_err(file_action_error)?;
        Ok(devcoordinator2_api::results::DeploymentRemoved {
            deployment_id: target.deployment_id,
            removed: true,
            data_deleted: delete_data,
            deleted_volumes,
        })
    }

    pub fn expire_previews(&self) -> Result<Vec<DeploymentStatus>, ProtocolError> {
        let now = self.store.current_timestamp()?;
        let mut expired = Vec::new();
        for row in self.store.list(None)? {
            if row
                .ttl_expires_at
                .as_deref()
                .is_some_and(|expires| expires < now.as_str())
                && row.state != "stopped"
            {
                let caller = Caller {
                    pid: 0,
                    uid: row.created_by_uid,
                    gid: primary_gid(row.created_by_uid).map_err(systemd_error)?,
                    client_kind: devcoordinator2_api::ClientKind::Other,
                    client_session: None,
                    work: None,
                    identity: None,
                };
                if let Ok(status) =
                    self.control("stop", None, None, Some(&row.deployment_id), None, &caller)
                {
                    expired.push(status);
                }
            }
        }
        Ok(expired)
    }

    pub fn control_observed(
        &self,
        action: &str,
        deployment_id: &str,
        component: Option<&str>,
    ) -> Result<DeploymentStatus, ProtocolError> {
        if !matches!(action, "start" | "stop" | "restart") {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "observed action is invalid",
            ));
        }
        let containers = self.observed_containers(deployment_id, component)?;
        let mut errors = Vec::new();
        for container in &containers {
            let exact = crate::docker::ExactContainerId::parse(container.0.clone())
                .map_err(|_| observed_action_error("recorded container identity is invalid"))?;
            if self.docker.container_state(&exact).state == RuntimeState::Missing {
                errors.push(format!(
                    "{}: recorded container no longer exists; re-import or adopt it",
                    container.1
                ));
                continue;
            }
            let result = match action {
                "start" => self.docker.start_container(&exact),
                "stop" => self.docker.stop_container(&exact),
                _ => self.docker.restart_container(&exact),
            };
            if let Err(error) = result {
                errors.push(format!("{}: {error}", container.1));
            }
        }
        self.refresh_observed_states(deployment_id)?;
        if !errors.is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentActionFailed,
                format!(
                    "{action} failed for {} container(s): {}",
                    errors.len(),
                    truncate(&errors.join("; "), 900)
                ),
            ));
        }
        self.store
            .observed_status(deployment_id)?
            .ok_or_else(|| not_found(deployment_id))
    }

    pub fn observed_logs(
        &self,
        deployment_id: &str,
        component: &str,
        tail_lines: u16,
    ) -> Result<DeploymentLog, ProtocolError> {
        let containers = self.observed_containers(deployment_id, Some(component))?;
        let mut tails = Vec::new();
        for (container_id, _) in containers {
            let exact = crate::docker::ExactContainerId::parse(container_id)
                .map_err(|_| observed_action_error("recorded container identity is invalid"))?;
            tails.push(
                self.docker
                    .container_logs(&exact, tail_lines)
                    .unwrap_or_else(|_| "(logs unavailable)".into()),
            );
        }
        Ok(DeploymentLog {
            deployment_id: Some(deployment_id.into()),
            component: component.into(),
            tail: truncate_tail(&tails.join("\n"), 65_536),
            truncated_before_tail: Some(true),
            log_path: None,
            container_id: None,
            observed_only: Some(true),
        })
    }

    pub fn reject_observed_configuration(&self, deployment_id: &str) -> Result<(), ProtocolError> {
        if self.store.observed_exists(deployment_id)? {
            Err(ProtocolError::new(
                ErrorCode::ObservedOnly,
                "observed deployments can be controlled and inspected, but apply, rollback, remove, and recreation require reviewed repository configuration",
            ))
        } else {
            Ok(())
        }
    }

    fn acquire_busy(&self, deployment_id: &str) -> Result<BusyGuard, ProtocolError> {
        let mut active = self
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(deployment_id.to_owned()) {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                format!("deployment {deployment_id} has a mutation in progress"),
            ));
        }
        Ok(BusyGuard {
            deployment_id: deployment_id.to_owned(),
            active: Arc::clone(&self.busy),
        })
    }

    fn control_compose_service(
        &self,
        action: &str,
        target: &DeploymentTarget,
        component_name: &str,
        service: &str,
    ) -> Result<DeploymentStatus, ProtocolError> {
        let specification = target
            .specification
            .component(component_name)
            .filter(|component| component.kind == ComponentKind::Compose)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!("no Compose component {component_name:?}"),
                )
            })?;
        if !specification
            .independent_services
            .iter()
            .any(|item| item == service)
        {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentActionFailed,
                format!(
                    "Compose service {component_name}/{service} is not independently controllable"
                ),
            ));
        }
        let stored = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .find(|component| component.name == component_name)
            .filter(|component| component.binding_kind.as_deref() == Some("compose"))
            .and_then(|component| component.binding_identity)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    format!("Compose component {component_name:?} has no managed project"),
                )
            })?;
        let result = (|| {
            if matches!(action, "stop" | "restart") {
                self.docker
                    .compose_stop_exact_services(&stored, &[service.to_owned()])?;
                if action == "stop" {
                    self.store
                        .set_compose_service_desired(
                            &target.deployment_id,
                            component_name,
                            service,
                            "stopped",
                        )
                        .map_err(protocol_as_docker)?;
                }
            }
            if matches!(action, "start" | "restart") {
                self.store
                    .set_compose_service_desired(
                        &target.deployment_id,
                        component_name,
                        service,
                        "running",
                    )
                    .map_err(protocol_as_docker)?;
                self.docker
                    .compose_start_exact_services(&stored, &[service.to_owned()])?;
                let (ready, note) = self.docker.compose_service_ready(
                    &stored,
                    service,
                    Duration::from_secs(specification.compose_timeout_seconds),
                )?;
                if !ready {
                    return Err(crate::docker::DockerError::Command(note));
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.store.set_component_runtime(
                &target.deployment_id,
                component_name,
                ComponentRuntimePatch {
                    last_error: Some(Some(truncate(&error.to_string(), 512))),
                    ..Default::default()
                },
            )?;
            self.store.patch_deployment_runtime(
                &target.deployment_id,
                DeploymentRuntimePatch {
                    state: Some("degraded".into()),
                    ..Default::default()
                },
            )?;
            return Err(ProtocolError::new(
                ErrorCode::DeploymentActionFailed,
                format!(
                    "{action} {component_name}/{service} failed: {}",
                    truncate(&error.to_string(), 512)
                ),
            ));
        }
        self.store.set_component_runtime(
            &target.deployment_id,
            component_name,
            ComponentRuntimePatch {
                last_error: Some(None),
                ..Default::default()
            },
        )?;
        let row = self
            .store
            .get(&target.deployment_id)?
            .ok_or_else(|| not_found(&target.deployment_id))?;
        let status = self.managed_status(&ResolvedDeployment {
            row,
            specification: target.specification.clone(),
        })?;
        self.store.patch_deployment_runtime(
            &target.deployment_id,
            DeploymentRuntimePatch {
                state: Some(status.state.clone()),
                ..Default::default()
            },
        )?;
        Ok(status)
    }

    fn stop_components(
        &self,
        target: &DeploymentTarget,
        row: &DeploymentRow,
        components: &[ComponentSpec],
    ) -> Result<(), ProtocolError> {
        let generation = row.current_generation.unwrap_or(0);
        let generation_path = self
            .store
            .generation(&target.deployment_id, generation)?
            .map_or_else(|| target.worktree.clone(), |generation| generation.path);
        let stored = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .map(|component| (component.name.clone(), component))
            .collect::<BTreeMap<_, _>>();
        for component in components {
            let Some(binding) = stored.get(&component.name) else {
                continue;
            };
            if !DeploymentStore::is_owned(component) {
                continue;
            }
            let Some(identity) = binding.binding_identity.as_deref() else {
                continue;
            };
            let kind = binding.binding_kind.as_deref().unwrap_or("");
            let runtime_generation = self.runtime_generation(row, binding)?;
            if let Err(error) = self.stop_binding(
                target,
                kind,
                identity,
                Some(component),
                &generation_path,
                runtime_generation,
            ) {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &component.name,
                    ComponentRuntimePatch {
                        state: Some("failed".into()),
                        last_error: Some(Some(truncate(&error.message, 512))),
                        ..Default::default()
                    },
                )?;
                return Err(ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    format!("stop {} failed: {}", component.name, error.message),
                ));
            }
            self.store.set_component_runtime(
                &target.deployment_id,
                &component.name,
                ComponentRuntimePatch {
                    desired_state: Some("stopped".into()),
                    state: Some("stopped".into()),
                    health: Some("none".into()),
                    last_error: Some(None),
                    ..Default::default()
                },
            )?;
        }
        if let Some(route) = target.specification.route_component()
            && components
                .iter()
                .any(|component| component.name == route.name)
        {
            self.store.set_route(
                DeploymentStore::effective_domain(Some(row), &target.specification, &target.source)
                    .as_deref(),
                &target.deployment_id,
                Some(&route.name),
                None,
                row.current_generation,
            )?;
            self.routes.publish_current()?;
        }
        Ok(())
    }

    fn start_components(
        &self,
        target: &DeploymentTarget,
        row: &DeploymentRow,
        components: &[ComponentSpec],
    ) -> Result<(), ProtocolError> {
        let generation = row.current_generation.unwrap_or(0);
        let generation_path = self
            .store
            .generation(&target.deployment_id, generation)?
            .map_or_else(|| target.worktree.clone(), |generation| generation.path);
        let mut port_map = crate::ports::assigned(&self.database, &target.deployment_id, 0)
            .map_err(runtime_error)?;
        port_map.extend(
            crate::ports::assigned(&self.database, &target.deployment_id, generation)
                .map_err(runtime_error)?,
        );
        let stored = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .map(|component| (component.name.clone(), component))
            .collect::<BTreeMap<_, _>>();
        for component in components {
            if !DeploymentStore::is_owned(component) {
                continue;
            }
            self.set_running_intent(target, std::slice::from_ref(component))?;
            let old = stored.get(&component.name);
            let started = (|| {
                if let Some(identity) = old
                    .filter(|component| component.binding_kind.as_deref() == Some("container"))
                    .and_then(|component| component.binding_identity.as_deref())
                    .and_then(|identity| ExactContainerId::parse(identity.to_owned()).ok())
                    .filter(|identity| {
                        self.docker.container_state(identity).state != RuntimeState::Missing
                    })
                {
                    self.docker
                        .start_container(&identity)
                        .map_err(runtime_error)?;
                    Ok(("container".into(), identity.to_string()))
                } else if let Some(project) = old
                    .filter(|component| component.binding_kind.as_deref() == Some("compose"))
                    .and_then(|component| component.binding_identity.as_deref())
                {
                    let services = component
                        .services
                        .iter()
                        .filter(|service| !component.finite_services.contains(*service))
                        .cloned()
                        .collect::<Vec<_>>();
                    if component.finite_services.is_empty() {
                        let context =
                            self.compose_context(target, component, &generation_path, generation)?;
                        self.docker
                            .compose_start(&context, &services)
                            .map_err(runtime_error)?;
                    } else if !services.is_empty() {
                        self.docker
                            .compose_start_exact_services(project, &services)
                            .map_err(runtime_error)?;
                    }
                    Ok(("compose".into(), project.to_owned()))
                } else {
                    self.start_component(target, component, generation, &generation_path, &port_map)
                }
            })();
            let binding = match started {
                Ok(binding) => binding,
                Err(error) => {
                    self.store.set_component_runtime(
                        &target.deployment_id,
                        &component.name,
                        ComponentRuntimePatch {
                            desired_state: Some("running".into()),
                            state: Some("failed".into()),
                            health: Some("unhealthy".into()),
                            last_error: Some(Some(truncate(&error.message, 512))),
                            ..Default::default()
                        },
                    )?;
                    return Err(ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        format!("start {} failed: {}", component.name, error.message),
                    ));
                }
            };
            let readiness =
                self.prove_health(target, component, &binding, &port_map, generation)?;
            self.store.set_component_runtime(
                &target.deployment_id,
                &component.name,
                ComponentRuntimePatch {
                    desired_state: Some("running".into()),
                    state: Some(
                        if !readiness.ready {
                            "failed"
                        } else if component.is_finite_workload() {
                            "completed"
                        } else {
                            "running"
                        }
                        .into(),
                    ),
                    health: Some(
                        if readiness.ready {
                            "healthy"
                        } else {
                            "unhealthy"
                        }
                        .into(),
                    ),
                    binding_kind: Some(Some(binding.0)),
                    binding_identity: Some(Some(binding.1)),
                    last_error: Some((!readiness.ready).then_some(readiness.note.clone())),
                    ..Default::default()
                },
            )?;
            if !readiness.ready {
                return Err(ProtocolError::new(
                    ErrorCode::DeploymentActionFailed,
                    format!(
                        "component {} unhealthy after start: {}",
                        component.name, readiness.note
                    ),
                ));
            }
        }
        if let Some(route) = target.specification.route_component()
            && components
                .iter()
                .any(|component| component.name == route.name)
        {
            self.store.set_route(
                DeploymentStore::effective_domain(Some(row), &target.specification, &target.source)
                    .as_deref(),
                &target.deployment_id,
                Some(&route.name),
                port_map.get(&route.name).copied(),
                Some(generation),
            )?;
            self.routes.publish_current()?;
        }
        Ok(())
    }

    fn recompute_state(&self, target: &DeploymentTarget) -> Result<(), ProtocolError> {
        let states = self
            .store
            .components(&target.deployment_id)?
            .into_iter()
            .filter(|component| {
                target
                    .specification
                    .component(&component.name)
                    .is_some_and(DeploymentStore::is_owned)
            })
            .map(|component| component.state)
            .collect::<Vec<_>>();
        let state = if !states.is_empty() && states.iter().all(|state| state == "completed") {
            "completed"
        } else if !states.is_empty()
            && states
                .iter()
                .all(|state| matches!(state.as_str(), "running" | "completed"))
        {
            "running"
        } else if states
            .iter()
            .all(|state| matches!(state.as_str(), "stopped" | "completed"))
        {
            "stopped"
        } else {
            "degraded"
        };
        self.store.patch_deployment_runtime(
            &target.deployment_id,
            DeploymentRuntimePatch {
                state: Some(state.into()),
                ..Default::default()
            },
        )
    }

    fn runtime_generation(
        &self,
        row: &DeploymentRow,
        component: &ComponentRow,
    ) -> Result<u32, ProtocolError> {
        if let Some(generation) = row.current_generation.filter(|value| *value > 0) {
            return Ok(generation);
        }
        if let Some(generation) = component.generation.filter(|value| *value > 0) {
            return Ok(generation);
        }
        Ok(self
            .store
            .generations(&row.deployment_id)?
            .into_iter()
            .map(|generation| generation.number)
            .max()
            .unwrap_or(0))
    }

    fn snapshot_desired(
        &self,
        deployment_id: &str,
        rows: &BTreeMap<String, ComponentRow>,
    ) -> Result<DesiredSnapshot, ProtocolError> {
        let components = rows
            .iter()
            .map(|(name, row)| (name.clone(), row.desired_state.clone()))
            .collect();
        let mut services = BTreeMap::new();
        for (name, row) in rows {
            if row.kind == "compose" {
                services.insert(
                    name.clone(),
                    self.store.compose_service_desires(deployment_id, name)?,
                );
            }
        }
        Ok((components, services))
    }

    fn restore_desired(
        &self,
        target: &DeploymentTarget,
        desired: &DesiredSnapshot,
    ) -> Result<(), ProtocolError> {
        for (name, state) in &desired.0 {
            if target.specification.component(name).is_some() {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    name,
                    ComponentRuntimePatch {
                        desired_state: Some(state.clone()),
                        ..Default::default()
                    },
                )?;
            }
        }
        for (component, services) in &desired.1 {
            let Some(specification) = target.specification.component(component) else {
                continue;
            };
            if specification.kind != ComponentKind::Compose {
                continue;
            }
            for (service, state) in services {
                if specification.independent_services.contains(service) {
                    self.store.set_compose_service_desired(
                        &target.deployment_id,
                        component,
                        service,
                        state,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn restore_after_preparation_failure(
        &self,
        target: &DeploymentTarget,
        old_components: &BTreeMap<String, ComponentRow>,
        desired: &DesiredSnapshot,
    ) -> Result<(), ProtocolError> {
        if let Some(row) = &target.row {
            return self.store.restore_apply_snapshot(
                row,
                &old_components.values().cloned().collect::<Vec<_>>(),
                &desired.1,
            );
        }
        self.store.patch_deployment_runtime(
            &target.deployment_id,
            DeploymentRuntimePatch {
                state: Some(
                    target
                        .row
                        .as_ref()
                        .map_or_else(|| "failed".into(), |row| row.state.clone()),
                ),
                ..Default::default()
            },
        )?;
        self.restore_desired(target, desired)
    }

    fn prepare_generation(
        &self,
        target: &DeploymentTarget,
        generation: u32,
        commit: Option<&str>,
    ) -> Result<PathBuf, ProtocolError> {
        self.files
            .ensure_layout(&target.deployment_id, target.caller_uid, target.caller_gid)
            .map_err(file_apply_error)?;
        if target.source == "worktree" {
            return Ok(target.worktree.clone());
        }
        let commit = commit.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                "checkout source needs a committed HEAD",
            )
        })?;
        self.files
            .allow_checkout_generation_creation(
                &target.deployment_id,
                target.caller_uid,
                target.caller_gid,
            )
            .map_err(file_apply_error)?;
        let generation_path = self
            .files
            .generation_path(&target.deployment_id, generation)
            .map_err(file_apply_error)?;
        self.git
            .add_detached_worktree(
                &target.worktree,
                &generation_path,
                commit,
                target.caller_uid,
                target.caller_gid,
            )
            .map_err(git_apply_error)?;
        Ok(generation_path)
    }

    fn remove_generation_path(&self, target: &DeploymentTarget, generation: u32, path: &Path) {
        if target.source != "checkout" {
            return;
        }
        let Ok(expected) = self
            .files
            .generation_path(&target.deployment_id, generation)
        else {
            return;
        };
        if path != expected {
            // A rollback generation can deliberately point at a retained
            // earlier checkout. Never delete that shared recovery path while
            // cleaning a failed synthetic generation row.
            return;
        }
        let _ =
            self.git
                .remove_worktree(&target.worktree, path, target.caller_uid, target.caller_gid);
        let _ = self
            .files
            .remove_generation_tree(&target.deployment_id, generation);
    }

    fn remove_recorded_generation_path(&self, target: &DeploymentTarget, path: &Path) {
        if target.source != "checkout" {
            return;
        }
        let Ok(directory) = self.files.deployment_dir(&target.deployment_id) else {
            return;
        };
        if path.parent() != Some(directory.as_path()) {
            return;
        }
        let Some(generation) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("gen-"))
            .and_then(|number| number.parse::<u32>().ok())
            .filter(|number| *number > 0)
        else {
            return;
        };
        let _ =
            self.git
                .remove_worktree(&target.worktree, path, target.caller_uid, target.caller_gid);
        let _ = self
            .files
            .remove_generation_tree(&target.deployment_id, generation);
    }

    fn run_build(
        &self,
        target: &DeploymentTarget,
        generation: u32,
        generation_path: &Path,
    ) -> Result<(), ProtocolError> {
        if self.finite_operations.cancelled(&target.deployment_id) {
            return Err(ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                "finite workload cancelled before build",
            ));
        }
        if target.specification.build.is_empty() {
            return Ok(());
        }
        let scratch = self
            .files
            .ensure_scratch(
                &target.deployment_id,
                generation,
                target.caller_uid,
                target.caller_gid,
            )
            .map_err(file_apply_error)?;
        let log = self
            .files
            .open_log_writer(
                &target.deployment_id,
                "build",
                target.caller_uid,
                target.caller_gid,
            )
            .map_err(file_apply_error)?;
        let specification = TransientUnitSpec {
            unit: format!(
                "{}-{}-build-g{generation}.service",
                self.config.deploy_unit_prefix(),
                target.deployment_id
            ),
            slice_name: deployment_slice(&self.config.deploy_unit_prefix(), &target.deployment_id),
            uid: target.caller_uid,
            gid: target.caller_gid,
            timeout_seconds: BUILD_TIMEOUT,
            working_directory: generation_path.to_owned(),
            environment_file: None,
            command: target
                .specification
                .build
                .iter()
                .map(OsString::from)
                .collect(),
            scratch_directory: scratch,
        };
        let mut child = self
            .systemd
            .spawn_transient(&specification)
            .map_err(|error| apply_runtime_error("build could not start", error))?;
        let log = Arc::new(Mutex::new(log));
        let stdout = child
            .take_stdout()
            .map(|stream| spawn_log_drain(stream, Arc::clone(&log)));
        let stderr = child
            .take_stderr()
            .map(|stream| spawn_log_drain(stream, Arc::clone(&log)));
        let status = if self.finite_operations.flag(&target.deployment_id).is_some() {
            loop {
                if self.finite_operations.cancelled(&target.deployment_id) {
                    let cgroup = self
                        .systemd
                        .control_group_path(&specification.unit)
                        .map_err(systemd_error)?;
                    self.systemd
                        .stop_unit(&specification.unit)
                        .map_err(systemd_error)?;
                    if !self
                        .systemd
                        .prove_cgroup_empty(cgroup.as_deref(), Duration::from_secs(15))
                    {
                        return Err(ProtocolError::new(
                            ErrorCode::DeploymentApplyFailed,
                            "cancelled build cleanup is not confirmed",
                        ));
                    }
                    break child.wait();
                }
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Err(error) => break Err(error),
                    Ok(None) => thread::sleep(Duration::from_millis(100)),
                }
            }
        } else {
            child.wait()
        }
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                "build process could not be reaped",
            )
            .with_detail(truncate(&error.to_string(), 512))
        })?;
        for drain in [stdout, stderr].into_iter().flatten() {
            drain.join().map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "build log reader stopped unexpectedly",
                )
            })??;
        }
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sync_all()
            .map_err(|error| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "build log could not be synchronized",
                )
                .with_detail(truncate(&error.to_string(), 512))
            })?;
        if status.success() && !self.finite_operations.cancelled(&target.deployment_id) {
            Ok(())
        } else {
            Err(ProtocolError::new(
                ErrorCode::DeploymentApplyFailed,
                format!("build exited {}; inspect the deployment build log", status),
            ))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn converge(
        &self,
        target: &DeploymentTarget,
        old_components: &BTreeMap<String, ComponentRow>,
        generation: u32,
        generation_path: &Path,
        domain: Option<&str>,
        rollback: Option<(u32, u32)>,
        desired: DesiredSnapshot,
    ) -> Result<DeploymentStatus, ProtocolError> {
        let port_map = self.allocate_ports(target, generation)?;
        let mut started = Vec::new();
        let mut failed_component = None;
        let convergence = (|| {
            for component in &target.specification.components {
                let binding = match self.bring_up(
                    target,
                    component,
                    generation,
                    generation_path,
                    &port_map,
                    old_components,
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        failed_component = Some(component.name.clone());
                        if DeploymentStore::is_owned(component)
                            && component.kind == ComponentKind::Compose
                        {
                            let identity = compose_project(&target.deployment_id, &component.name);
                            started.push(StartedBinding {
                                specification: component.clone(),
                                kind: "compose".into(),
                                identity: identity.clone(),
                            });
                            self.store.set_component_runtime(
                                &target.deployment_id,
                                &component.name,
                                ComponentRuntimePatch {
                                    spec_fingerprint: Some(DeploymentStore::component_fingerprint(
                                        component,
                                    )),
                                    state: Some("failed".into()),
                                    health: Some("unhealthy".into()),
                                    generation: Some(Some(generation)),
                                    binding_kind: Some(Some("compose".into())),
                                    binding_identity: Some(Some(identity)),
                                    last_error: Some(Some(truncate(&error.message, 512))),
                                    ..Default::default()
                                },
                            )?;
                        }
                        return Err(error);
                    }
                };
                let old = old_components.get(&component.name);
                let newly_started = binding.0 != "none"
                    && (component.is_finite_workload()
                        || DeploymentStore::is_generation_scoped(component)
                        || old.is_none()
                        || old.and_then(|row| row.binding_kind.as_deref())
                            != Some(binding.0.as_str())
                        || old.and_then(|row| row.binding_identity.as_deref())
                            != Some(binding.1.as_str()));
                if newly_started {
                    started.push(StartedBinding {
                        specification: component.clone(),
                        kind: binding.0.clone(),
                        identity: binding.1.clone(),
                    });
                    self.store.set_component_runtime(
                        &target.deployment_id,
                        &component.name,
                        ComponentRuntimePatch {
                            spec_fingerprint: Some(DeploymentStore::component_fingerprint(
                                component,
                            )),
                            state: Some("starting".into()),
                            health: Some("unknown".into()),
                            generation: Some(Some(generation)),
                            binding_kind: Some(Some(binding.0.clone())),
                            binding_identity: Some(Some(binding.1.clone())),
                            last_error: Some(None),
                            ..Default::default()
                        },
                    )?;
                }
                let readiness =
                    self.prove_health(target, component, &binding, &port_map, generation)?;
                let stored_generation = if DeploymentStore::is_generation_scoped(component) {
                    generation
                } else {
                    0
                };
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &component.name,
                    ComponentRuntimePatch {
                        spec_fingerprint: Some(DeploymentStore::component_fingerprint(component)),
                        state: Some(
                            if !readiness.ready {
                                "failed"
                            } else if component.is_finite_workload() {
                                "completed"
                            } else {
                                "running"
                            }
                            .into(),
                        ),
                        health: Some(
                            if readiness.ready {
                                "healthy"
                            } else {
                                "unhealthy"
                            }
                            .into(),
                        ),
                        generation: Some(Some(stored_generation)),
                        binding_kind: Some(Some(binding.0.clone())),
                        binding_identity: Some(Some(binding.1.clone())),
                        last_error: Some((!readiness.ready).then_some(readiness.note.clone())),
                        ..Default::default()
                    },
                )?;
                if !readiness.ready {
                    return Err(ProtocolError::new(
                        ErrorCode::DeploymentApplyFailed,
                        format!(
                            "component {} unhealthy: {}",
                            component.name,
                            truncate(&readiness.note, 512)
                        ),
                    ));
                }
            }
            if !self.finite_operations.seal(&target.deployment_id) {
                return Err(ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "finite workload cancelled before commit",
                ));
            }
            Ok(())
        })();

        if let Err(error) = convergence {
            self.abort_candidate(
                target,
                generation,
                generation_path,
                &started,
                old_components,
                &desired,
            )?;
            if let Some(name) = failed_component
                && self
                    .store
                    .components(&target.deployment_id)?
                    .iter()
                    .any(|component| component.name == name)
            {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &name,
                    ComponentRuntimePatch {
                        last_error: Some(Some(truncate(&error.message, 512))),
                        ..Default::default()
                    },
                )?;
            }
            let _ = self.validate_or_withdraw_route(&target.deployment_id);
            let had_generation = target
                .row
                .as_ref()
                .and_then(|row| row.current_generation)
                .is_some();
            self.store.patch_deployment_runtime(
                &target.deployment_id,
                DeploymentRuntimePatch {
                    state: Some(
                        if self.finite_operations.cancelled(&target.deployment_id) {
                            "cancelled"
                        } else if had_generation {
                            "degraded"
                        } else {
                            "failed"
                        }
                        .into(),
                    ),
                    ..Default::default()
                },
            )?;
            let components = self
                .store
                .components(&target.deployment_id)?
                .into_iter()
                .map(|component| {
                    serde_json::json!({
                        "name": component.name,
                        "state": component.state,
                        "health": component.health,
                        "last_error": component.last_error,
                    })
                })
                .collect::<Vec<_>>();
            let message = error.message;
            return Err(
                ProtocolError::new(ErrorCode::DeploymentApplyFailed, message.clone()).with_detail(
                    serde_json::json!({"failed": message, "components": components}).to_string(),
                ),
            );
        }

        if let (Some(domain), Some(component)) = (domain, target.specification.route_component()) {
            let port = port_map.get(&component.name).copied().ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "routed component has no allocated port",
                )
            })?;
            self.store.set_route(
                Some(domain),
                &target.deployment_id,
                Some(&component.name),
                Some(port),
                Some(generation),
            )?;
        } else {
            self.store
                .set_route(None, &target.deployment_id, None, None, None)?;
        }
        self.routes.publish_current()?;
        let previous = target.row.as_ref().and_then(|row| row.current_generation);
        self.retire_previous(
            target,
            old_components,
            previous,
            generation,
            generation_path,
        );
        let mut keep = BTreeSet::from([generation]);
        if target.source == "checkout"
            && let Some(previous) = previous
        {
            keep.insert(previous);
        }
        let protected_paths = self
            .store
            .generations(&target.deployment_id)?
            .into_iter()
            .filter(|row| keep.contains(&row.number))
            .map(|row| row.path)
            .collect::<HashSet<_>>();
        for stale in self.store.prune_generations(&target.deployment_id, &keep)? {
            if !protected_paths.contains(&stale.path) {
                self.remove_recorded_generation_path(target, &stale.path);
            }
        }
        if target.source != "checkout"
            && let Some(previous) = previous
        {
            crate::ports::release(&self.database, &target.deployment_id, Some(previous), None)
                .map_err(runtime_error)?;
        }
        self.store
            .set_generation_state(&target.deployment_id, generation, "current")?;
        if target.source == "checkout"
            && let Some(previous) = previous
        {
            self.store
                .set_generation_state(&target.deployment_id, previous, "previous")?;
        }
        self.set_running_intent(target, &target.specification.components)?;
        self.store.patch_deployment_runtime(
            &target.deployment_id,
            DeploymentRuntimePatch {
                state: Some(
                    if target
                        .specification
                        .components
                        .iter()
                        .all(ComponentSpec::is_finite_workload)
                    {
                        "completed"
                    } else {
                        "running"
                    }
                    .into(),
                ),
                current_generation: Some(Some(generation)),
                previous_generation: Some(if target.source == "checkout" {
                    previous
                } else {
                    None
                }),
                ..Default::default()
            },
        )?;
        let row = self
            .store
            .get(&target.deployment_id)?
            .ok_or_else(|| not_found(&target.deployment_id))?;
        let mut status = self.managed_status(&ResolvedDeployment {
            row,
            specification: target.specification.clone(),
        })?;
        if let Some((from, to)) = rollback {
            status.rolled_back_from = Some(from);
            status.rolled_back_to = Some(to);
        }
        Ok(status)
    }

    fn allocate_ports(
        &self,
        target: &DeploymentTarget,
        generation: u32,
    ) -> Result<BTreeMap<String, u16>, ProtocolError> {
        let mut result = BTreeMap::new();
        let stable = crate::ports::assigned(&self.database, &target.deployment_id, 0)
            .map_err(runtime_error)?;
        let now = self.store.current_timestamp()?;
        for component in &target.specification.components {
            if !component.wants_port || !DeploymentStore::is_owned(component) {
                continue;
            }
            let port = if DeploymentStore::is_generation_scoped(component) {
                crate::ports::lease_with_availability(
                    &self.database,
                    self.config.port_range,
                    &target.deployment_id,
                    &component.name,
                    generation,
                    &now,
                    self.port_availability.as_ref(),
                )
                .map_err(runtime_error)?
            } else if let Some(port) = stable.get(&component.name) {
                *port
            } else {
                crate::ports::lease_with_availability(
                    &self.database,
                    self.config.port_range,
                    &target.deployment_id,
                    &component.name,
                    0,
                    &now,
                    self.port_availability.as_ref(),
                )
                .map_err(runtime_error)?
            };
            result.insert(component.name.clone(), port);
        }
        Ok(result)
    }

    fn abort_candidate(
        &self,
        target: &DeploymentTarget,
        generation: u32,
        generation_path: &Path,
        started: &[StartedBinding],
        old_components: &BTreeMap<String, ComponentRow>,
        desired: &DesiredSnapshot,
    ) -> Result<(), ProtocolError> {
        for binding in started.iter().rev() {
            let stopped = self.stop_binding(
                target,
                &binding.kind,
                &binding.identity,
                Some(&binding.specification),
                generation_path,
                generation,
            );
            if binding.specification.is_finite_workload()
                && let Err(error) = stopped
            {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &binding.specification.name,
                    ComponentRuntimePatch {
                        state: Some("failed".into()),
                        health: Some("unhealthy".into()),
                        last_error: Some(Some("finite workload cleanup is not confirmed".into())),
                        ..Default::default()
                    },
                )?;
                self.store.patch_deployment_runtime(
                    &target.deployment_id,
                    DeploymentRuntimePatch {
                        state: Some("failed".into()),
                        ..Default::default()
                    },
                )?;
                return Err(error);
            }
            if binding.kind == "container"
                && DeploymentStore::is_generation_scoped(&binding.specification)
                && let Ok(identity) = ExactContainerId::parse(binding.identity.clone())
            {
                let _ = self.docker.remove_container(&identity, false);
            }
            if let Some(old) = old_components.get(&binding.specification.name) {
                self.restore_component(old)?;
            } else if DeploymentStore::is_generation_scoped(&binding.specification) {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &binding.specification.name,
                    ComponentRuntimePatch {
                        state: Some("stopped".into()),
                        health: Some("none".into()),
                        generation: Some(None),
                        binding_kind: Some(None),
                        binding_identity: Some(None),
                        ..Default::default()
                    },
                )?;
            } else {
                self.store.set_component_runtime(
                    &target.deployment_id,
                    &binding.specification.name,
                    ComponentRuntimePatch {
                        state: Some("stopped".into()),
                        health: Some("none".into()),
                        generation: Some(Some(generation)),
                        binding_kind: Some(Some(binding.kind.clone())),
                        binding_identity: Some(Some(binding.identity.clone())),
                        ..Default::default()
                    },
                )?;
            }
        }
        crate::ports::release(
            &self.database,
            &target.deployment_id,
            Some(generation),
            None,
        )
        .map_err(runtime_error)?;
        self.remove_generation_path(target, generation, generation_path);
        self.store
            .set_generation_state(&target.deployment_id, generation, "failed")?;
        let mut keep = old_components
            .values()
            .filter_map(|component| component.generation)
            .collect::<BTreeSet<_>>();
        keep.insert(0);
        self.store.prune_generations(&target.deployment_id, &keep)?;
        if let Some(row) = &target.row {
            self.store.restore_apply_snapshot(
                row,
                &old_components.values().cloned().collect::<Vec<_>>(),
                &desired.1,
            )
        } else {
            self.restore_desired(target, desired)
        }
    }

    fn restore_component(&self, component: &ComponentRow) -> Result<(), ProtocolError> {
        self.store.set_component_runtime(
            &component.deployment_id,
            &component.name,
            ComponentRuntimePatch {
                spec_fingerprint: Some(component.spec_fingerprint.clone()),
                desired_state: Some(component.desired_state.clone()),
                state: Some(component.state.clone()),
                health: Some(component.health.clone()),
                generation: Some(component.generation),
                binding_kind: Some(component.binding_kind.clone()),
                binding_identity: Some(component.binding_identity.clone()),
                restarts: Some(component.restarts),
                last_error: Some(component.last_error.clone()),
            },
        )
    }

    fn retire_previous(
        &self,
        target: &DeploymentTarget,
        old_components: &BTreeMap<String, ComponentRow>,
        previous: Option<u32>,
        generation: u32,
        generation_path: &Path,
    ) {
        let mut rows = old_components.values().collect::<Vec<_>>();
        rows.sort_by_key(|component| component.order_index);
        for old in rows.into_iter().rev() {
            let specification = target.specification.component(&old.name);
            let scoped = specification.is_some_and(DeploymentStore::is_generation_scoped);
            let removed = specification.is_none();
            if old.binding_identity.is_none()
                || (!scoped && !removed)
                || (scoped && old.generation == Some(generation))
            {
                continue;
            }
            let kind = old.binding_kind.as_deref().unwrap_or("");
            let identity = old.binding_identity.as_deref().unwrap_or("");
            let _ = self.stop_binding(
                target,
                kind,
                identity,
                specification,
                generation_path,
                old.generation.unwrap_or(0),
            );
            if kind == "container"
                && (scoped || removed)
                && let Ok(identity) = ExactContainerId::parse(identity.to_owned())
            {
                let _ = self.docker.remove_container(&identity, false);
            }
        }
        if target.source == "checkout"
            && let Some(previous) = previous
        {
            let _ =
                crate::ports::release(&self.database, &target.deployment_id, Some(previous), None);
        }
    }

    fn set_running_intent(
        &self,
        target: &DeploymentTarget,
        components: &[ComponentSpec],
    ) -> Result<(), ProtocolError> {
        for component in components {
            self.store.set_component_runtime(
                &target.deployment_id,
                &component.name,
                ComponentRuntimePatch {
                    desired_state: Some("running".into()),
                    ..Default::default()
                },
            )?;
            if component.kind == ComponentKind::Compose {
                for service in &component.independent_services {
                    self.store.set_compose_service_desired(
                        &target.deployment_id,
                        &component.name,
                        service,
                        "running",
                    )?;
                }
            }
        }
        Ok(())
    }

    fn bring_up(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation: u32,
        generation_path: &Path,
        port_map: &BTreeMap<String, u16>,
        old_components: &BTreeMap<String, ComponentRow>,
    ) -> Result<(String, String), ProtocolError> {
        if !DeploymentStore::is_owned(component) {
            return Ok(("none".into(), String::new()));
        }
        if DeploymentStore::is_generation_scoped(component) {
            return self.start_component(target, component, generation, generation_path, port_map);
        }
        let old = old_components.get(&component.name);
        let unchanged = old.is_some_and(|old| {
            old.binding_identity.is_some()
                && old.spec_fingerprint == DeploymentStore::component_fingerprint(component)
        });
        if unchanged
            && matches!(
                component.kind,
                ComponentKind::Docker | ComponentKind::Postgres
            )
            && let Some(identity) = old.and_then(|old| old.binding_identity.as_deref())
            && let Ok(identity) = ExactContainerId::parse(identity.to_owned())
        {
            let state = self.docker.container_state(&identity);
            if state.state == RuntimeState::Running {
                return Ok(("container".into(), identity.to_string()));
            }
            if state.state != RuntimeState::Missing
                && self.docker.start_container(&identity).is_ok()
            {
                return Ok(("container".into(), identity.to_string()));
            }
        }
        self.start_component(target, component, generation, generation_path, port_map)
    }

    fn start_component(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation: u32,
        generation_path: &Path,
        port_map: &BTreeMap<String, u16>,
    ) -> Result<(String, String), ProtocolError> {
        self.files
            .ensure_layout(&target.deployment_id, target.caller_uid, target.caller_gid)
            .map_err(file_apply_error)?;
        let environment = self.component_environment(target, component, generation, port_map)?;
        let process_environment = component.kind == ComponentKind::Process;
        let environment_path = self
            .files
            .write_environment(
                &target.deployment_id,
                &component.name,
                generation,
                &environment,
                if process_environment {
                    EnvironmentFormat::Systemd
                } else {
                    EnvironmentFormat::Docker
                },
                if process_environment {
                    target.caller_uid
                } else {
                    rustix::process::geteuid().as_raw()
                },
                if process_environment {
                    target.caller_gid
                } else {
                    rustix::process::getegid().as_raw()
                },
            )
            .map_err(file_apply_error)?;
        match component.kind {
            ComponentKind::Process => {
                let unit = process_unit_name(
                    &self.config.deploy_unit_prefix(),
                    &target.deployment_id,
                    &component.name,
                    generation,
                );
                let working_directory = generation_path.join(&component.cwd);
                let working_directory = working_directory.canonicalize().map_err(|error| {
                    ProtocolError::new(
                        ErrorCode::DeploymentApplyFailed,
                        format!(
                            "component {} working directory is unavailable",
                            component.name
                        ),
                    )
                    .with_detail(truncate(&error.to_string(), 512))
                })?;
                if !working_directory.starts_with(generation_path) {
                    return Err(ProtocolError::new(
                        ErrorCode::DeploymentApplyFailed,
                        format!(
                            "component {} working directory escapes its generation",
                            component.name
                        ),
                    ));
                }
                let log_path = self
                    .files
                    .log_path(&target.deployment_id, &component.name)
                    .map_err(file_apply_error)?;
                self.systemd
                    .start_persistent(&PersistentUnitSpec {
                        unit: unit.clone(),
                        slice_name: deployment_slice(
                            &self.config.deploy_unit_prefix(),
                            &target.deployment_id,
                        ),
                        uid: target.caller_uid,
                        gid: target.caller_gid,
                        working_directory,
                        environment_file: environment_path,
                        command: component.command.iter().map(OsString::from).collect(),
                        log_path,
                    })
                    .map_err(|error| {
                        apply_runtime_error(
                            &format!("component {} failed to start", component.name),
                            error,
                        )
                    })?;
                Ok(("unit".into(), unit))
            }
            ComponentKind::Docker | ComponentKind::Postgres => {
                let identity = self.start_container_component(
                    target,
                    component,
                    generation,
                    port_map,
                    environment_path,
                )?;
                Ok(("container".into(), identity.to_string()))
            }
            ComponentKind::Compose => {
                let project = compose_project(&target.deployment_id, &component.name);
                let context =
                    self.compose_context(target, component, generation_path, generation)?;
                let mut log = self
                    .files
                    .open_log_append(
                        &target.deployment_id,
                        "build",
                        target.caller_uid,
                        target.caller_gid,
                    )
                    .map_err(file_apply_error)?;
                writeln!(
                    log,
                    "\nGeneration {generation}: Compose component {}",
                    component.name
                )
                .map_err(|error| apply_runtime_error("cannot record Compose build", error))?;
                let log = Arc::new(Mutex::new(log));
                let result = self.docker.compose_up(
                    &context,
                    &component.services,
                    &component.finite_services,
                    component.compose_build,
                    Some(Arc::clone(&log)),
                    self.finite_operations.flag(&target.deployment_id),
                );
                let mut writer = log
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(error) = &result {
                    writeln!(writer, "{error}").map_err(|error| {
                        apply_runtime_error("cannot record Compose failure", error)
                    })?;
                }
                writer.sync_all().map_err(|error| {
                    apply_runtime_error("cannot synchronize Compose build log", error)
                })?;
                result.map_err(|error| ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    format!("compose component {} failed ({:?}); inspect deployment logs with component build", component.name, error.kind()),
                ))?;
                Ok(("compose".into(), project))
            }
            ComponentKind::External => Ok(("none".into(), String::new())),
        }
    }

    fn start_container_component(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation: u32,
        port_map: &BTreeMap<String, u16>,
        mut environment_path: PathBuf,
    ) -> Result<ExactContainerId, ProtocolError> {
        let mut name = container_name(&target.deployment_id, &component.name);
        if DeploymentStore::is_generation_scoped(component) {
            name.push_str(&format!("-g{generation}"));
        } else {
            let labels = BTreeMap::from([
                (
                    "devcoordinator2.deployment".into(),
                    target.deployment_id.clone(),
                ),
                ("devcoordinator2.component".into(), component.name.clone()),
            ]);
            for existing in self
                .docker
                .list_ids_by_labels(&labels)
                .map_err(|error| apply_runtime_error("cannot inspect stable component", error))?
            {
                self.docker
                    .remove_container(&existing, false)
                    .map_err(|error| {
                        apply_runtime_error("cannot replace stable component", error)
                    })?;
            }
        }
        let mut publish = Vec::new();
        let mut volumes = Vec::new();
        let mut command = component.command.clone();
        let image = component.image.as_deref().unwrap_or("");
        if component.kind == ComponentKind::Postgres {
            let credentials = self
                .files
                .postgres_credentials(
                    &target.deployment_id,
                    &component.name,
                    component.user.as_deref().unwrap_or("app"),
                    component.database.as_deref().unwrap_or("app"),
                )
                .map_err(file_apply_error)?;
            environment_path = self
                .files
                .write_postgres_environment(
                    &target.deployment_id,
                    &component.name,
                    generation,
                    &credentials,
                )
                .map_err(file_apply_error)?;
            let port = port_map.get(&component.name).copied().ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    format!(
                        "PostgreSQL component {} has no allocated port",
                        component.name
                    ),
                )
            })?;
            publish.push(format!("127.0.0.1:{port}:5432"));
            volumes.push(format!(
                "{}:/var/lib/postgresql/data",
                managed_volume_name(&target.deployment_id, &component.name, "pgdata")
            ));
            command.clear();
        } else {
            if let (Some(container_port), Some(host_port)) = (
                component.container_port,
                port_map.get(&component.name).copied(),
            ) {
                publish.push(format!("127.0.0.1:{host_port}:{container_port}"));
            }
            for volume in &component.volumes {
                let (declared, destination) = volume.split_once(':').ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryConfigInvalid,
                        format!("component {} contains an invalid volume", component.name),
                    )
                })?;
                volumes.push(format!(
                    "{}:{destination}",
                    managed_volume_name(&target.deployment_id, &component.name, declared)
                ));
            }
        }
        self.docker
            .ensure_image(image)
            .map_err(|error| apply_runtime_error("component image is unavailable", error))?;
        self.docker
            .create_container(&CreateContainerRequest {
                name,
                image: image.into(),
                label_context: self.label_context(target, component, generation),
                labels: BTreeMap::new(),
                env_file: Some(environment_path),
                publish,
                volumes,
                command,
                restart: "on-failure:5".into(),
            })
            .and_then(|identity| {
                self.docker.start_container(&identity)?;
                Ok(identity)
            })
            .map_err(|error| apply_runtime_error("container component failed to start", error))
    }

    fn label_context(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation: u32,
    ) -> ManagedLabelContext {
        ManagedLabelContext {
            instance: self.config.unit_prefix.clone(),
            repository_id: target.repository_id.clone(),
            worktree_id: target.worktree_id.clone(),
            run_id: None,
            deployment_id: Some(target.deployment_id.clone()),
            component: Some(component.name.clone()),
            generation: Some(u64::from(generation)),
            ttl_seconds: target.specification.ttl_seconds,
            purpose: if target.specification.ttl_seconds.is_some() {
                "preview"
            } else {
                "permanent"
            }
            .into(),
            caller_uid: target.caller_uid,
            client: target.client.clone(),
            session: target.session.clone(),
            created_at: self
                .store
                .current_timestamp()
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into()),
            data_class: if component.owns_persistent_data() {
                "persistent"
            } else {
                "disposable"
            }
            .into(),
        }
    }

    fn component_environment(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation: u32,
        port_map: &BTreeMap<String, u16>,
    ) -> Result<BTreeMap<String, String>, ProtocolError> {
        let mut environment = component.env.clone();
        environment.insert("DC2_DEPLOYMENT".into(), target.deployment_id.clone());
        environment.insert("DC2_COMPONENT".into(), component.name.clone());
        environment.insert("DC2_GENERATION".into(), generation.to_string());
        if component.wants_port
            && let Some(port) = port_map.get(&component.name)
        {
            environment.insert("PORT".into(), port.to_string());
        }
        for (name, port) in port_map {
            environment.insert(
                format!("DC2_PORT_{}", environment_suffix(name)),
                port.to_string(),
            );
        }
        let mut postgres = BTreeMap::new();
        for other in target
            .specification
            .components
            .iter()
            .filter(|component| component.kind == ComponentKind::Postgres)
        {
            if let Some(url) = self.postgres_url(target, other, port_map)? {
                postgres.insert(other.name.clone(), url.clone());
                environment.insert(
                    format!("DC2_POSTGRES_{}_URL", environment_suffix(&other.name)),
                    url,
                );
            }
        }
        if postgres.len() == 1 {
            environment
                .entry("DATABASE_URL".into())
                .or_insert_with(|| postgres.into_values().next().unwrap());
        }
        Ok(environment)
    }

    fn postgres_url(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        port_map: &BTreeMap<String, u16>,
    ) -> Result<Option<String>, ProtocolError> {
        let (credentials, port) = if let Some(shared) = &component.shared_from {
            let (deployment, component) = shared.split_once('/').ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryConfigInvalid,
                    "shared PostgreSQL target is invalid",
                )
            })?;
            let Some(credentials) = self
                .files
                .read_postgres_credentials(deployment, component)
                .map_err(file_apply_error)?
            else {
                return Ok(None);
            };
            let Some(port) = crate::ports::assigned(&self.database, deployment, 0)
                .map_err(runtime_error)?
                .get(component)
                .copied()
            else {
                return Ok(None);
            };
            (credentials, port)
        } else {
            let credentials = self
                .files
                .postgres_credentials(
                    &target.deployment_id,
                    &component.name,
                    component.user.as_deref().unwrap_or("app"),
                    component.database.as_deref().unwrap_or("app"),
                )
                .map_err(file_apply_error)?;
            let Some(port) = port_map.get(&component.name).copied() else {
                return Ok(None);
            };
            (credentials, port)
        };
        Ok(Some(format!(
            "postgresql://{}:{}@127.0.0.1:{}/{}",
            credentials.user, credentials.password, port, credentials.database
        )))
    }

    fn compose_context(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        generation_path: &Path,
        generation: u32,
    ) -> Result<ComposeContext, ProtocolError> {
        let files = component
            .compose_files
            .iter()
            .map(|file| generation_path.join(file))
            .collect::<Vec<_>>();
        let mut env_files = Vec::new();
        if let Some(relative) = &component.compose_env_file {
            if !self
                .configuration
                .authorized(&target.repository_id, relative)
            {
                return Err(ProtocolError::new(
                    ErrorCode::AuthorizationRequired,
                    format!(
                        "Compose env_file {relative:?} is not authorized by private instance configuration"
                    ),
                ));
            }
            let path = self
                .files
                .validate_repository_file(generation_path, relative)
                .map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::RepositoryConfigInvalid,
                        format!("Compose env_file {relative:?} is unavailable or unsafe"),
                    )
                })?;
            if !self
                .git
                .is_ignored(
                    generation_path,
                    relative,
                    target.caller_uid,
                    target.caller_gid,
                )
                .map_err(git_apply_error)?
            {
                return Err(ProtocolError::new(
                    ErrorCode::RepositoryConfigInvalid,
                    format!("Compose env_file {relative:?} must remain ignored"),
                ));
            }
            env_files.push(path);
        }
        env_files.push(
            self.files
                .environment_path(&target.deployment_id, &component.name, generation)
                .map_err(file_apply_error)?,
        );
        Ok(ComposeContext {
            project: compose_project(&target.deployment_id, &component.name),
            files,
            cwd: generation_path.to_owned(),
            env_files,
        })
    }

    fn prove_health(
        &self,
        target: &DeploymentTarget,
        component: &ComponentSpec,
        binding: &(String, String),
        port_map: &BTreeMap<String, u16>,
        generation: u32,
    ) -> Result<Readiness, ProtocolError> {
        if component.kind == ComponentKind::External {
            let (host, port) = component
                .tcp
                .as_deref()
                .and_then(|value| value.rsplit_once(':'))
                .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryConfigInvalid,
                        "external TCP target is invalid",
                    )
                })?;
            return Ok(self
                .health
                .tcp_ready(host, port, Duration::from_secs(10), &|| None));
        }
        if component.kind == ComponentKind::Postgres {
            let (identity, credentials) = if let Some(shared) = &component.shared_from {
                let (deployment, name) = shared.split_once('/').ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryConfigInvalid,
                        "shared PostgreSQL target is invalid",
                    )
                })?;
                let identity = self
                    .store
                    .components(deployment)?
                    .into_iter()
                    .find(|row| row.name == name)
                    .and_then(|row| row.binding_identity)
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::DeploymentApplyFailed,
                            "shared PostgreSQL component is not deployed",
                        )
                    })?;
                let credentials = self
                    .files
                    .read_postgres_credentials(deployment, name)
                    .map_err(file_apply_error)?
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::DeploymentApplyFailed,
                            "shared PostgreSQL credentials are unavailable",
                        )
                    })?;
                (identity, credentials)
            } else {
                let credentials = self
                    .files
                    .read_postgres_credentials(&target.deployment_id, &component.name)
                    .map_err(file_apply_error)?
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::DeploymentApplyFailed,
                            "PostgreSQL credentials are unavailable",
                        )
                    })?;
                (binding.1.clone(), credentials)
            };
            let identity = ExactContainerId::parse(identity).map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "PostgreSQL binding identity is invalid",
                )
            })?;
            return Ok(self.postgres_ready(&identity, &credentials, Duration::from_secs(120)));
        }

        let routed_compose_port = if binding.0 == "compose"
            && target
                .specification
                .route_component()
                .is_some_and(|route| route.name == component.name)
        {
            let port = port_map.get(&component.name).copied().ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "routed Compose component has no allocated host port",
                )
            })?;
            let (published, note) = self
                .docker
                .compose_publishes_host_port(&binding.1, port)
                .map_err(|error| apply_runtime_error("cannot verify Compose route", error))?;
            if !published {
                return Ok(Readiness::failed(note));
            }
            Some(port)
        } else {
            None
        };
        let timeout = Duration::from_secs(
            component
                .health
                .as_ref()
                .map_or(30, |health| health.timeout_seconds),
        );
        let launched = Instant::now();
        let terminal = || self.terminal_binding(&binding.0, &binding.1, launched);
        if let Some(health) = &component.health
            && let Some(port) = port_map.get(&component.name).copied()
        {
            return Ok(match health.kind {
                crate::repository_config::HealthKind::Http => self.health.http_ready(
                    port,
                    health.path.as_deref().unwrap_or("/"),
                    timeout,
                    &terminal,
                ),
                crate::repository_config::HealthKind::Tcp => {
                    self.health.tcp_ready("127.0.0.1", port, timeout, &terminal)
                }
            });
        }
        match binding.0.as_str() {
            "unit" => {
                while launched.elapsed() < Duration::from_secs(1) {
                    if let Some(reason) = terminal() {
                        return Ok(Readiness::failed(reason));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let state = self
                    .systemd
                    .process_state(&binding.1)
                    .map_err(systemd_error)?;
                Ok(if state.state == "running" {
                    Readiness::ready(format!("unit {}", state.active_state))
                } else {
                    Readiness::failed(format!("unit {}", state.active_state))
                })
            }
            "container" => {
                let identity = ExactContainerId::parse(binding.1.clone()).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::DeploymentApplyFailed,
                        "container binding identity is invalid",
                    )
                })?;
                let state = self.docker.container_state(&identity);
                Ok(
                    if state.state == RuntimeState::Running
                        || (state.status.as_deref() == Some("running")
                            && state.health.as_deref() == Some("starting"))
                    {
                        Readiness::ready("container running")
                    } else {
                        Readiness::failed(format!(
                            "container {}",
                            state.status.as_deref().unwrap_or(state.state.as_str())
                        ))
                    },
                )
            }
            "compose" => {
                let completions = self.store.compose_completions(
                    &target.deployment_id,
                    &component.name,
                    generation,
                )?;
                let desires = self
                    .store
                    .compose_service_desires(&target.deployment_id, &component.name)?
                    .into_iter()
                    .map(|(service, state)| {
                        (
                            service,
                            if state == "stopped" {
                                RuntimeState::Stopped
                            } else {
                                RuntimeState::Running
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let (ready, note, state) = self
                    .docker
                    .compose_ready(
                        &binding.1,
                        &component.services,
                        &component.finite_services,
                        &completions.keys().cloned().collect(),
                        &desires,
                        Duration::from_secs(component.compose_timeout_seconds),
                        self.finite_operations.flag(&target.deployment_id),
                    )
                    .map_err(|error| apply_runtime_error("Compose health check failed", error))?;
                let candidates = state
                    .completion_candidates
                    .into_iter()
                    .map(|candidate| ComposeCompletionInput {
                        service: candidate.service,
                        container_id: candidate.container_id.to_string(),
                        image_id: candidate.state.image_id,
                        exit_code: candidate.state.exit_code.unwrap_or(0),
                        started_at: candidate.state.started_at,
                        finished_at: candidate.state.finished_at,
                    })
                    .collect::<Vec<_>>();
                self.store.record_compose_completions(
                    &target.deployment_id,
                    &component.name,
                    generation,
                    &candidates,
                )?;
                if ready && let Some(port) = routed_compose_port {
                    return Ok(self.health.tcp_ready(
                        "127.0.0.1",
                        port,
                        Duration::from_secs(component.compose_timeout_seconds),
                        &terminal,
                    ));
                }
                Ok(if ready {
                    Readiness::ready(note)
                } else {
                    Readiness::failed(note)
                })
            }
            _ => Ok(Readiness::ready("no check")),
        }
    }

    fn postgres_ready(
        &self,
        identity: &ExactContainerId,
        credentials: &PostgresCredentials,
        timeout: Duration,
    ) -> Readiness {
        let deadline = Instant::now() + timeout;
        let arguments = vec![
            "pg_isready".into(),
            "-h".into(),
            "127.0.0.1".into(),
            "-U".into(),
            credentials.user.clone(),
            "-d".into(),
            credentials.database.clone(),
        ];
        let mut consecutive = 0;
        while Instant::now() < deadline {
            if self
                .docker
                .exec_ok(identity, &arguments, Duration::from_secs(30))
            {
                consecutive += 1;
                if consecutive >= 2 {
                    return Readiness::ready("accepting connections");
                }
            } else {
                consecutive = 0;
            }
            thread::sleep(Duration::from_millis(100));
        }
        Readiness::failed("pg_isready never succeeded")
    }

    fn terminal_binding(&self, kind: &str, identity: &str, launched: Instant) -> Option<String> {
        if launched.elapsed() < Duration::from_secs(1) {
            return None;
        }
        match kind {
            "unit" => self
                .systemd
                .process_state(identity)
                .ok()
                .filter(|state| matches!(state.state.as_str(), "failed" | "stopped"))
                .map(|state| {
                    format!(
                        "unit became terminal: {}/{} ({})",
                        state.active_state,
                        state.sub_state,
                        if state.result.is_empty() {
                            "no result"
                        } else {
                            &state.result
                        }
                    )
                }),
            "container" => ExactContainerId::parse(identity.to_owned())
                .ok()
                .map(|identity| self.docker.container_state(&identity))
                .filter(|state| {
                    matches!(
                        state.state,
                        RuntimeState::Failed | RuntimeState::Stopped | RuntimeState::Missing
                    )
                })
                .map(|state| {
                    format!(
                        "container became terminal: {}",
                        state.status.as_deref().unwrap_or(state.state.as_str())
                    )
                }),
            _ => None,
        }
    }

    fn stop_binding(
        &self,
        target: &DeploymentTarget,
        kind: &str,
        identity: &str,
        specification: Option<&ComponentSpec>,
        generation_path: &Path,
        generation: u32,
    ) -> Result<(), ProtocolError> {
        match kind {
            "unit" => {
                let cgroup = self
                    .systemd
                    .control_group_path(identity)
                    .map_err(systemd_error)?;
                self.systemd.stop_unit(identity).map_err(systemd_error)?;
                if !self
                    .systemd
                    .prove_cgroup_empty(cgroup.as_deref(), Duration::from_secs(15))
                {
                    return Err(ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        format!("cgroup of {identity} still has processes after stop"),
                    ));
                }
                self.systemd.reset_failed(identity).map_err(systemd_error)
            }
            "container" => {
                let identity = ExactContainerId::parse(identity.to_owned()).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::DeploymentActionFailed,
                        "recorded container identity is invalid",
                    )
                })?;
                self.docker.stop_container(&identity).map_err(runtime_error)
            }
            "compose" => {
                let Some(specification) = specification else {
                    return Ok(());
                };
                let context =
                    self.compose_context(target, specification, generation_path, generation)?;
                self.docker.compose_stop(&context).map_err(runtime_error)?;
                if specification.is_finite_workload() {
                    let state = self
                        .docker
                        .compose_state(
                            &context.project,
                            &specification.services,
                            &specification.finite_services,
                            &BTreeSet::new(),
                            &BTreeMap::new(),
                        )
                        .map_err(runtime_error)?;
                    if state.services.iter().any(|service| {
                        matches!(
                            service.state,
                            RuntimeState::Running | RuntimeState::Starting
                        )
                    }) {
                        return Err(ProtocolError::new(
                            ErrorCode::DeploymentActionFailed,
                            "finite workload still runs after stop",
                        ));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn validate_or_withdraw_route(&self, deployment_id: &str) -> Result<bool, ProtocolError> {
        let deployment_id_owned = deployment_id.to_owned();
        let port = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT port FROM domain_routes WHERE deployment_id=?1",
                        [&deployment_id_owned],
                        |row| row.get::<_, Option<u16>>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .flatten();
        let Some(port) = port else {
            return Ok(false);
        };
        if self.health.tcp_probe("127.0.0.1", port) {
            return Ok(true);
        }
        self.store
            .set_route(None, deployment_id, None, None, None)?;
        self.routes.publish_current()?;
        Ok(false)
    }

    fn resolve(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<ResolvedDeployment, ProtocolError> {
        let target = self.resolve_target(path, name, deployment_id, caller)?;
        let row = target.row.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::DeploymentNotFound,
                format!(
                    "{}@{} was never applied",
                    target.specification.name, target.source
                ),
            )
        })?;
        Ok(ResolvedDeployment {
            row,
            specification: target.specification,
        })
    }

    fn resolve_target(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentTarget, ProtocolError> {
        let target = self.resolve_target_readonly(path, name, deployment_id, caller)?;
        if deployment_id.is_none() {
            self.registry
                .register(&target.worktree, caller.uid, caller.gid)?;
        }
        Ok(target)
    }

    fn resolve_target_readonly(
        &self,
        path: Option<&str>,
        name: Option<&str>,
        deployment_id: Option<&str>,
        caller: &Caller,
    ) -> Result<DeploymentTarget, ProtocolError> {
        if let Some(deployment_id) = deployment_id {
            let row = self
                .store
                .get(deployment_id)?
                .ok_or_else(|| not_found(deployment_id))?;
            let worktree = self.worktree_path(&row.worktree_id)?;
            let specification = load_deployment_spec(&worktree, &row.name).map_err(config_error)?;
            if !specification
                .sources
                .iter()
                .any(|source| source == &row.source)
            {
                return Err(ProtocolError::new(
                    ErrorCode::RepositoryConfigInvalid,
                    "recorded deployment source is no longer declared",
                ));
            }
            let (caller_uid, caller_gid) = if caller.identity.is_some() {
                (
                    row.created_by_uid,
                    primary_gid(row.created_by_uid).map_err(systemd_error)?,
                )
            } else {
                (caller.uid, caller.gid)
            };
            return Ok(DeploymentTarget {
                deployment_id: row.deployment_id.clone(),
                repository_id: row.repository_id.clone(),
                worktree_id: row.worktree_id.clone(),
                source: row.source.clone(),
                row: Some(row),
                worktree,
                specification,
                caller_uid,
                caller_gid,
                client: client_kind_name(caller.client_kind).into(),
                session: caller.client_session.clone(),
            });
        }
        if caller.identity.is_some() {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "public callers address deployments by deployment_id",
            ));
        }
        let path = path.ok_or_else(|| {
            ProtocolError::new(ErrorCode::ParamsInvalid, "path is required with name")
        })?;
        let selected = name.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "name or deployment_id is required",
            )
        })?;
        let resolved =
            crate::repository::resolve_worktree(Path::new(path), Some((caller.uid, caller.gid)))?;
        let repository_id = crate::ids::repository_id(&resolved.repository_root).map_err(|_| {
            ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                "cannot resolve repository identity",
            )
        })?;
        let worktree_id = crate::ids::worktree_id(&resolved.worktree_root).map_err(|_| {
            ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                "cannot resolve worktree identity",
            )
        })?;
        let worktree = resolved.worktree_root;
        let (name, explicit_source) = selected
            .split_once('@')
            .map_or((selected, None), |(name, source)| (name, Some(source)));
        let specification = load_deployment_spec(&worktree, name).map_err(config_error)?;
        let source = match explicit_source {
            Some(source) => source.to_owned(),
            None if specification.sources.len() == 1 => specification.sources[0].clone(),
            None => {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!(
                        "deployment {name:?} enables {:?}; address it as name@source",
                        specification.sources
                    ),
                ));
            }
        };
        if !specification.sources.contains(&source) {
            return Err(ProtocolError::new(
                ErrorCode::RepositoryConfigInvalid,
                format!("deployment {name:?} does not enable source {source:?}"),
            ));
        }
        let deployment_id = DeploymentStore::deployment_id(&worktree_id, name, &source);
        Ok(DeploymentTarget {
            row: self.store.get(&deployment_id)?,
            deployment_id,
            repository_id,
            worktree_id,
            worktree,
            specification,
            source,
            caller_uid: caller.uid,
            caller_gid: caller.gid,
            client: client_kind_name(caller.client_kind).into(),
            session: caller.client_session.clone(),
        })
    }

    fn managed_status(
        &self,
        resolved: &ResolvedDeployment,
    ) -> Result<DeploymentStatus, ProtocolError> {
        let row = &resolved.row;
        let component_rows = self.store.components(&row.deployment_id)?;
        let mut ports =
            crate::ports::assigned(&self.database, &row.deployment_id, 0).map_err(runtime_error)?;
        if let Some(generation) = row.current_generation {
            ports.extend(
                crate::ports::assigned(&self.database, &row.deployment_id, generation)
                    .map_err(runtime_error)?,
            );
        }
        let mut components = Vec::new();
        for component_row in component_rows {
            let specification = resolved.specification.component(&component_row.name);
            components.push(self.component_status(
                row,
                &component_row,
                specification,
                ports.get(&component_row.name).copied(),
            )?);
        }
        let owned = components
            .iter()
            .filter(|component| component.owned)
            .collect::<Vec<_>>();
        let expected_components = resolved
            .specification
            .components
            .iter()
            .map(|component| component.name.clone())
            .collect::<Vec<_>>();
        let missing_components = expected_components
            .iter()
            .filter(|name| !components.iter().any(|component| &component.name == *name))
            .cloned()
            .collect::<Vec<_>>();
        let mut state = if row.state == "cancelled" {
            "cancelled"
        } else if row.state == "applying" {
            "applying"
        } else if !missing_components.is_empty() {
            "degraded"
        } else if !owned.is_empty() && owned.iter().all(|component| component.state == "completed")
        {
            "completed"
        } else if !owned.is_empty()
            && owned
                .iter()
                .all(|component| matches!(component.state.as_str(), "running" | "completed"))
        {
            "running"
        } else if owned
            .iter()
            .all(|component| matches!(component.state.as_str(), "stopped" | "completed"))
        {
            "stopped"
        } else {
            "degraded"
        }
        .to_owned();
        if state == "running"
            && owned
                .iter()
                .any(|component| component.health == "unhealthy")
        {
            state = "degraded".into();
        }
        let deployment_id = row.deployment_id.clone();
        let (domain, route_port) = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT domain,port FROM domain_routes WHERE deployment_id=?1",
                        [&deployment_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<u16>>(1)?)),
                    )
                    .optional()
                    .map(|value| value.unwrap_or((String::new(), None)))
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        let repository_id = row.repository_id.clone();
        let repository_name = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT display_name FROM repositories WHERE repository_id=?1",
                        [&repository_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        Ok(DeploymentStatus {
            deployment_id: row.deployment_id.clone(),
            repository_id: row.repository_id.clone(),
            repository_name,
            name: resolved.specification.name.clone(),
            source: deployment_source(&row.source)?,
            state,
            health: None,
            current_generation: row.current_generation,
            previous_generation: row.previous_generation,
            domain: (!domain.is_empty()).then_some(domain),
            route_port,
            route_component: resolved
                .specification
                .route_component()
                .map(|component| component.name.clone()),
            public: row.public,
            ttl_expires_at: row.ttl_expires_at.clone(),
            components,
            // Private host paths are not part of the v2 normal result.
            log_dir: None,
            observed_only: false,
            native_project: None,
            observation_source: None,
            observed_at: None,
            unchanged: None,
            rolled_back_from: None,
            rolled_back_to: None,
            readiness: Some(devcoordinator2_api::results::DeploymentReadiness {
                ready: false,
                expected_components,
                missing_components,
                pending_apply: None,
                blockers: Vec::new(),
            }),
        })
    }

    fn component_status(
        &self,
        deployment: &DeploymentRow,
        row: &ComponentRow,
        specification: Option<&ComponentSpec>,
        port: Option<u16>,
    ) -> Result<Component, ProtocolError> {
        let (state, health, restarts, services) = match (
            row.binding_kind.as_deref(),
            row.binding_identity.as_deref(),
            specification,
        ) {
            (_, _, Some(specification)) if specification.kind == ComponentKind::External => {
                let (host, port) = specification
                    .tcp
                    .as_deref()
                    .and_then(|value| value.rsplit_once(':'))
                    .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::RepositoryConfigInvalid,
                            "external TCP target is invalid",
                        )
                    })?;
                let ready = self.network.tcp(host, port);
                (
                    if ready { "running" } else { "failed" }.into(),
                    if ready { "healthy" } else { "unhealthy" }.into(),
                    0,
                    None,
                )
            }
            (_, None, _) => (row.state.clone(), row.health.clone(), row.restarts, None),
            (Some("unit"), Some(identity), _) => {
                let live = self
                    .systemd
                    .process_state(identity)
                    .map_err(systemd_error)?;
                let health = live_health(row, &live.state);
                (live.state, health, live.restarts, None)
            }
            (Some("container"), Some(identity), _) => {
                let exact = crate::docker::ExactContainerId::parse(identity.to_owned())
                    .map_err(|_| observed_action_error("recorded container identity is invalid"))?;
                let live = self.docker.container_state(&exact);
                let state = live.state.as_str().to_owned();
                let health = live_health(row, &state);
                (state, health, live.restarts, None)
            }
            (Some("compose"), Some(project), Some(specification)) => {
                let generation = deployment.current_generation.unwrap_or(0);
                let completions = self.store.compose_completions(
                    &deployment.deployment_id,
                    &row.name,
                    generation,
                )?;
                let mut desires = self
                    .store
                    .compose_service_desires(&deployment.deployment_id, &row.name)?;
                if row.desired_state == "stopped" {
                    for service in specification
                        .services
                        .iter()
                        .filter(|service| !specification.finite_services.contains(*service))
                    {
                        desires.insert(service.clone(), "stopped".into());
                    }
                }
                let desired = desires
                    .into_iter()
                    .map(|(service, state)| {
                        (
                            service,
                            if state == "stopped" {
                                RuntimeState::Stopped
                            } else {
                                RuntimeState::Running
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let live = self
                    .docker
                    .compose_state(
                        project,
                        &specification.services,
                        &specification.finite_services,
                        &completions.keys().cloned().collect::<BTreeSet<_>>(),
                        &desired,
                    )
                    .map_err(runtime_error)?;
                let services = live
                    .services
                    .into_iter()
                    .map(|service| ComposeService {
                        independent: specification.independent_services.contains(&service.name),
                        name: service.name,
                        role: service.role,
                        state: service.state.as_str().into(),
                        desired_state: service.desired_state.as_str().into(),
                        containers: service.containers,
                    })
                    .collect::<Vec<_>>();
                let state = live.state.as_str().to_owned();
                let health = live_health(row, &state);
                (state, health, 0, Some(services))
            }
            _ => (row.state.clone(), row.health.clone(), row.restarts, None),
        };
        let completed_services = if specification
            .is_some_and(|specification| specification.kind == ComponentKind::Compose)
        {
            let generation = deployment.current_generation.unwrap_or(0);
            let rows =
                self.store
                    .compose_completions(&deployment.deployment_id, &row.name, generation)?;
            (!rows.is_empty()).then(|| rows.into_values().collect())
        } else {
            None
        };
        Ok(Component {
            name: row.name.clone(),
            display_name: None,
            r#type: row.kind.clone(),
            state,
            health,
            generation: row.generation,
            binding: ComponentBinding {
                kind: row.binding_kind.clone(),
                identity: row.binding_identity.clone(),
            },
            port,
            restarts: Some(restarts),
            owned: specification.is_some_and(DeploymentStore::is_owned),
            independent_control: specification
                .is_some_and(|specification| specification.independent_control),
            last_error: row.last_error.clone(),
            services,
            completed_services,
        })
    }

    fn observed_containers(
        &self,
        deployment_id: &str,
        component: Option<&str>,
    ) -> Result<Vec<(String, String)>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        let component = component.map(str::to_owned);
        let rows = self.database.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT container_id,compose_service FROM observed_containers WHERE observed_deployment_id=?1 ORDER BY compose_service,name,container_id",
            )?;
            Ok(statement.query_map([deployment_id], |row| {
                Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))
            })?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(database_error)?;
        let rows = component.as_ref().map_or(rows.clone(), |component| {
            rows.into_iter()
                .filter(|(_, service)| service == component)
                .collect()
        });
        if rows.is_empty() {
            return Err(if let Some(component) = component {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    format!("no observed component {component:?}"),
                )
            } else {
                observed_action_error("no containers are recorded for this observed deployment")
            });
        }
        Ok(rows)
    }

    fn refresh_observed_states(&self, deployment_id: &str) -> Result<(), ProtocolError> {
        let containers = self.observed_containers(deployment_id, None)?;
        let mut states = Vec::new();
        let mut rows = Vec::new();
        for (container_id, _) in containers {
            let exact = crate::docker::ExactContainerId::parse(container_id.clone())
                .map_err(|_| observed_action_error("recorded container identity is invalid"))?;
            let live = self.docker.container_state(&exact);
            let state = live.state.as_str().to_owned();
            let health = if live.state != RuntimeState::Running {
                "none".to_owned()
            } else {
                match live.health.as_deref() {
                    Some("healthy") => "healthy",
                    Some("unhealthy") => "unhealthy",
                    Some("starting") => "starting",
                    _ => "unknown",
                }
                .to_owned()
            };
            states.push((state.clone(), health.clone()));
            rows.push((
                container_id,
                state,
                live.status.unwrap_or_else(|| live.state.as_str().into()),
                health,
            ));
        }
        let deployment_state = if states.iter().all(|(state, _)| state == "running") {
            "running"
        } else if states
            .iter()
            .all(|(state, _)| matches!(state.as_str(), "stopped" | "missing"))
        {
            "stopped"
        } else if states.iter().any(|(state, _)| state == "failed")
            && !states.iter().any(|(state, _)| state == "running")
        {
            "failed"
        } else {
            "degraded"
        }
        .to_owned();
        let deployment_health = if states.iter().any(|(_, health)| health == "unhealthy") {
            "unhealthy"
        } else if states.iter().all(|(_, health)| health == "healthy") {
            "healthy"
        } else {
            "unknown"
        }
        .to_owned();
        let now = self.store.current_timestamp()?;
        let deployment_id = deployment_id.to_owned();
        self.database.transaction(move |transaction| {
            for (container_id,state,status,health) in rows {
                transaction.execute(
                    "UPDATE observed_containers SET state=?1,status=?2,health=?3,observed_at=?4 WHERE container_id=?5",
                    rusqlite::params![state,status,health,now,container_id],
                )?;
            }
            transaction.execute(
                "UPDATE observed_deployments SET state=?1,health=?2,observed_at=?3 WHERE observed_deployment_id=?4",
                rusqlite::params![deployment_state,deployment_health,now,deployment_id],
            )?;
            Ok(())
        }).map_err(database_error)
    }

    fn worktree_path(&self, worktree_id: &str) -> Result<PathBuf, ProtocolError> {
        let worktree_id = worktree_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT worktree_path FROM worktrees WHERE worktree_id=?1",
                        [&worktree_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .map(PathBuf::from)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::DeploymentNotFound,
                    "deployment worktree is unregistered",
                )
            })
    }
}

fn live_health(row: &ComponentRow, state: &str) -> String {
    if state == "completed" {
        "healthy".into()
    } else if state == "running" {
        row.health.clone()
    } else if row.state == "running" {
        "unhealthy".into()
    } else {
        "none".into()
    }
}

fn deployment_source(value: &str) -> Result<DeploymentSource, ProtocolError> {
    match value {
        "worktree" => Ok(DeploymentSource::Worktree),
        "checkout" => Ok(DeploymentSource::Checkout),
        "observed" => Ok(DeploymentSource::Observed),
        _ => Err(ProtocolError::new(
            ErrorCode::InternalError,
            "stored deployment source is invalid",
        )),
    }
}

fn config_error(error: crate::repository_config::RepositoryConfigError) -> ProtocolError {
    ProtocolError::new(ErrorCode::RepositoryConfigInvalid, error.to_string())
}

fn not_found(deployment_id: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentNotFound,
        format!("no deployment {deployment_id}"),
    )
}

fn observed_action_error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::DeploymentActionFailed, message)
}

fn runtime_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentActionFailed,
        "deployment runtime is unavailable",
    )
    .with_detail(truncate(&error.to_string(), 512))
}

fn systemd_error(error: crate::systemd::SystemdError) -> ProtocolError {
    runtime_error(error)
}

fn git_apply_error(error: crate::deployment_git::DeploymentGitError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentApplyFailed,
        "deployment Git operation failed",
    )
    .with_detail(truncate(&error.to_string(), 512))
}

fn file_apply_error(error: crate::deployment_files::DeploymentFileError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentApplyFailed,
        "deployment runtime files could not be prepared",
    )
    .with_detail(truncate(&error.to_string(), 512))
}

fn file_action_error(error: crate::deployment_files::DeploymentFileError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentActionFailed,
        "deployment runtime files are unavailable",
    )
    .with_detail(truncate(&error.to_string(), 512))
}

fn apply_runtime_error(label: &str, error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::DeploymentApplyFailed,
        format!("{label}: {}", truncate(&error.to_string(), 512)),
    )
}

fn protocol_as_docker(error: ProtocolError) -> crate::docker::DockerError {
    crate::docker::DockerError::Command(error.message)
}

fn process_unit_name(
    prefix: &str,
    deployment_id: &str,
    component: &str,
    generation: u32,
) -> String {
    format!("{prefix}-{deployment_id}-{component}-g{generation}.service")
}

fn deployment_slice(prefix: &str, deployment_id: &str) -> String {
    format!("{prefix}-{deployment_id}.slice")
}

fn environment_suffix(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character == '-' {
                '_'
            } else {
                character.to_ascii_uppercase()
            }
        })
        .collect()
}

fn client_kind_name(kind: devcoordinator2_api::ClientKind) -> &'static str {
    match kind {
        devcoordinator2_api::ClientKind::Codex => "codex",
        devcoordinator2_api::ClientKind::Claude => "claude",
        devcoordinator2_api::ClientKind::Cursor => "cursor",
        devcoordinator2_api::ClientKind::Antigravity => "antigravity",
        devcoordinator2_api::ClientKind::Human => "human",
        devcoordinator2_api::ClientKind::Edge => "edge",
        devcoordinator2_api::ClientKind::Other => "other",
    }
}

fn spawn_log_drain(
    mut source: Box<dyn Read + Send>,
    destination: Arc<Mutex<std::fs::File>>,
) -> thread::JoinHandle<Result<(), ProtocolError>> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let count = source.read(&mut buffer).map_err(|error| {
                ProtocolError::new(
                    ErrorCode::DeploymentApplyFailed,
                    "build output could not be read",
                )
                .with_detail(truncate(&error.to_string(), 512))
            })?;
            if count == 0 {
                return Ok(());
            }
            destination
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .write_all(&buffer[..count])
                .map_err(|error| {
                    ProtocolError::new(
                        ErrorCode::DeploymentApplyFailed,
                        "build output could not be written",
                    )
                    .with_detail(truncate(&error.to_string(), 512))
                })?;
        }
    })
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(
            ErrorCode::InternalError,
            "deployment database operation failed",
        )
        .with_detail(other.to_string()),
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn truncate_tail(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deployment_state::{
        ComponentRuntimePatch, ComposeCompletionInput, ObservedContainerInput,
        ObservedDeploymentInput, RegisteredDeploymentTarget,
    };
    use crate::docker::{
        CompletionCandidate, ComposeServiceState, ComposeState as DockerComposeState,
        ContainerState, DockerError, DockerInvocation, DockerOutput, ExactContainerId, LogFollower,
    };
    use crate::systemd::{
        PersistentUnitSpec, ProcessState, SystemdError, TransientUnitSpec, UnitProcess,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::tempdir;
    use time::macros::datetime;

    struct FakeDocker {
        states: Mutex<HashMap<String, ContainerState>>,
        actions: Mutex<Vec<String>>,
        compose: Mutex<Option<DockerComposeState>>,
    }

    impl FakeDocker {
        fn new() -> Self {
            Self {
                states: Mutex::new(HashMap::new()),
                actions: Mutex::new(Vec::new()),
                compose: Mutex::new(None),
            }
        }

        fn running(service: Option<&str>) -> ContainerState {
            ContainerState {
                state: RuntimeState::Running,
                status: Some("running".into()),
                restarts: 2,
                exit_code: Some(0),
                health: Some("healthy".into()),
                started_at: Some("start".into()),
                finished_at: None,
                image_id: Some("sha256:image".into()),
                compose_service: service.map(str::to_owned),
            }
        }
    }

    impl DockerControl for FakeDocker {
        fn invoke(&self, _invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            Err(DockerError::InvalidRequest("unexpected invoke".into()))
        }

        fn spawn_follow_logs(
            &self,
            _container_id: &ExactContainerId,
        ) -> Result<LogFollower, DockerError> {
            Err(DockerError::InvalidRequest("unexpected logs".into()))
        }

        fn container_state(&self, container_id: &ExactContainerId) -> ContainerState {
            self.states
                .lock()
                .unwrap()
                .get(container_id.as_str())
                .cloned()
                .unwrap_or(ContainerState {
                    state: RuntimeState::Missing,
                    status: None,
                    restarts: 0,
                    exit_code: None,
                    health: None,
                    started_at: None,
                    finished_at: None,
                    image_id: None,
                    compose_service: None,
                })
        }

        fn start_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("start:{container_id}"));
            Ok(())
        }

        fn stop_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("stop:{container_id}"));
            Ok(())
        }

        fn restart_container(&self, container_id: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("restart:{container_id}"));
            Ok(())
        }

        fn container_logs(
            &self,
            container_id: &ExactContainerId,
            _tail_lines: u16,
        ) -> Result<String, DockerError> {
            Ok(format!("logs:{container_id}"))
        }

        fn compose_state(
            &self,
            _project: &str,
            _services: &[String],
            _finite_services: &[String],
            _completions: &BTreeSet<String>,
            _desired_states: &BTreeMap<String, RuntimeState>,
        ) -> Result<DockerComposeState, DockerError> {
            self.compose
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| DockerError::InvalidOutput("no compose fixture".into()))
        }
    }

    struct FakeSystemd;

    impl SystemdControl for FakeSystemd {
        fn spawn_transient(
            &self,
            _specification: &TransientUnitSpec,
        ) -> Result<Box<dyn UnitProcess>, SystemdError> {
            Err(SystemdError::Operation("unexpected spawn".into()))
        }

        fn start_persistent(
            &self,
            _specification: &PersistentUnitSpec,
        ) -> Result<(), SystemdError> {
            Err(SystemdError::Operation("unexpected start".into()))
        }

        fn process_state(&self, _unit: &str) -> Result<ProcessState, SystemdError> {
            Ok(ProcessState {
                state: "running".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
                result: "success".into(),
                main_pid: 42,
                restarts: 3,
                cgroup: "/fixture".into(),
            })
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
            Some([1000; 4])
        }
    }

    struct CompletedUnit {
        stdout: Option<Box<dyn Read + Send>>,
        stderr: Option<Box<dyn Read + Send>>,
        status: Option<std::process::ExitStatus>,
    }

    impl UnitProcess for CompletedUnit {
        fn id(&self) -> u32 {
            42
        }

        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            self.stdout.take()
        }

        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            self.stderr.take()
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            Ok(self.status)
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            self.status
                .ok_or_else(|| std::io::Error::other("fixture status is missing"))
        }
    }

    struct MutationSystemd {
        actions: Mutex<Vec<String>>,
        states: Mutex<HashMap<String, String>>,
        build_exit: std::sync::atomic::AtomicI32,
    }

    impl MutationSystemd {
        fn new() -> Self {
            Self {
                actions: Mutex::new(Vec::new()),
                states: Mutex::new(HashMap::new()),
                build_exit: std::sync::atomic::AtomicI32::new(0),
            }
        }
    }

    impl SystemdControl for MutationSystemd {
        fn spawn_transient(
            &self,
            specification: &TransientUnitSpec,
        ) -> Result<Box<dyn UnitProcess>, SystemdError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("build:{}", specification.unit));
            use std::os::unix::process::ExitStatusExt;
            let exit = self.build_exit.load(Ordering::SeqCst);
            Ok(Box::new(CompletedUnit {
                stdout: Some(Box::new(std::io::Cursor::new(b"build stdout\n".to_vec()))),
                stderr: Some(Box::new(std::io::Cursor::new(b"build stderr\n".to_vec()))),
                status: Some(std::process::ExitStatus::from_raw(exit << 8)),
            }))
        }

        fn start_persistent(&self, specification: &PersistentUnitSpec) -> Result<(), SystemdError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("start:{}", specification.unit));
            self.states
                .lock()
                .unwrap()
                .insert(specification.unit.clone(), "running".into());
            std::fs::write(&specification.log_path, "process output\n")
                .map_err(|error| SystemdError::Operation(error.to_string()))
        }

        fn process_state(&self, unit: &str) -> Result<ProcessState, SystemdError> {
            let state = self
                .states
                .lock()
                .unwrap()
                .get(unit)
                .cloned()
                .unwrap_or_else(|| "stopped".into());
            Ok(ProcessState {
                active_state: if state == "running" {
                    "active"
                } else {
                    "inactive"
                }
                .into(),
                state,
                sub_state: "fixture".into(),
                result: "success".into(),
                main_pid: 42,
                restarts: 0,
                cgroup: "/fixture".into(),
            })
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

        fn stop_unit(&self, unit: &str) -> Result<(), SystemdError> {
            self.actions.lock().unwrap().push(format!("stop:{unit}"));
            self.states
                .lock()
                .unwrap()
                .insert(unit.into(), "stopped".into());
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
            Some([rustix::process::getuid().as_raw(); 4])
        }
    }

    struct ReadyNetwork;
    impl NetworkProbe for ReadyNetwork {
        fn tcp(&self, _host: &str, _port: u16) -> bool {
            true
        }
    }

    struct MutationDocker {
        states: Mutex<HashMap<String, ContainerState>>,
        actions: Mutex<Vec<String>>,
        next: AtomicU64,
        fail_create: std::sync::atomic::AtomicBool,
        fail_compose: std::sync::atomic::AtomicBool,
        block_finite: std::sync::atomic::AtomicBool,
        finite_started: std::sync::atomic::AtomicBool,
    }

    impl MutationDocker {
        fn new() -> Self {
            Self {
                states: Mutex::new(HashMap::new()),
                actions: Mutex::new(Vec::new()),
                next: AtomicU64::new(1),
                fail_create: std::sync::atomic::AtomicBool::new(false),
                fail_compose: std::sync::atomic::AtomicBool::new(false),
                block_finite: std::sync::atomic::AtomicBool::new(false),
                finite_started: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn set_state(&self, identity: &ExactContainerId, state: RuntimeState) {
            let (status, health) = match state {
                RuntimeState::Running => ("running", Some("healthy".into())),
                RuntimeState::Stopped => ("exited", None),
                _ => (state.as_str(), None),
            };
            self.states.lock().unwrap().insert(
                identity.to_string(),
                ContainerState {
                    state,
                    status: Some(status.into()),
                    restarts: 0,
                    exit_code: Some(0),
                    health,
                    started_at: None,
                    finished_at: None,
                    image_id: Some("sha256:fixture".into()),
                    compose_service: None,
                },
            );
        }
    }

    impl DockerControl for MutationDocker {
        fn invoke(&self, _invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            Err(DockerError::InvalidRequest(
                "unexpected raw Docker call".into(),
            ))
        }

        fn spawn_follow_logs(
            &self,
            _container_id: &ExactContainerId,
        ) -> Result<LogFollower, DockerError> {
            Err(DockerError::InvalidRequest("unexpected follow".into()))
        }

        fn ensure_image(&self, image: &str) -> Result<(), DockerError> {
            self.actions.lock().unwrap().push(format!("image:{image}"));
            Ok(())
        }

        fn list_ids_by_labels(
            &self,
            _labels: &BTreeMap<String, String>,
        ) -> Result<Vec<ExactContainerId>, DockerError> {
            Ok(Vec::new())
        }

        fn create_container(
            &self,
            request: &CreateContainerRequest,
        ) -> Result<ExactContainerId, DockerError> {
            if self.fail_create.load(Ordering::SeqCst) {
                return Err(DockerError::Command("fixture create failure".into()));
            }
            let value = self.next.fetch_add(1, Ordering::SeqCst);
            let identity = ExactContainerId::parse(format!("{value:064x}"))?;
            self.actions.lock().unwrap().push(format!(
                "create:{}:{}",
                request
                    .label_context
                    .component
                    .as_deref()
                    .unwrap_or("unknown"),
                identity
            ));
            self.set_state(&identity, RuntimeState::Stopped);
            Ok(identity)
        }

        fn container_state(&self, identity: &ExactContainerId) -> ContainerState {
            self.states
                .lock()
                .unwrap()
                .get(identity.as_str())
                .cloned()
                .unwrap_or(ContainerState {
                    state: RuntimeState::Missing,
                    status: None,
                    restarts: 0,
                    exit_code: None,
                    health: None,
                    started_at: None,
                    finished_at: None,
                    image_id: None,
                    compose_service: None,
                })
        }

        fn start_container(&self, identity: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("start:{identity}"));
            self.set_state(identity, RuntimeState::Running);
            Ok(())
        }

        fn stop_container(&self, identity: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("stop:{identity}"));
            self.set_state(identity, RuntimeState::Stopped);
            Ok(())
        }

        fn restart_container(&self, identity: &ExactContainerId) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("restart:{identity}"));
            self.set_state(identity, RuntimeState::Running);
            Ok(())
        }

        fn remove_container(
            &self,
            identity: &ExactContainerId,
            _delete_volumes: bool,
        ) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("remove:{identity}"));
            self.states.lock().unwrap().remove(identity.as_str());
            Ok(())
        }

        fn container_logs(
            &self,
            identity: &ExactContainerId,
            _tail_lines: u16,
        ) -> Result<String, DockerError> {
            Ok(format!("managed log {identity}"))
        }

        fn exec_ok(
            &self,
            _container_id: &ExactContainerId,
            _argv: &[String],
            _timeout: Duration,
        ) -> bool {
            true
        }

        fn remove_volume(
            &self,
            volume: &crate::docker::ManagedVolumeName,
        ) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("volume-remove:{volume}"));
            Ok(())
        }

        fn compose_up(
            &self,
            context: &ComposeContext,
            _services: &[String],
            _finite_services: &[String],
            _build: bool,
            _output_log: Option<Arc<Mutex<std::fs::File>>>,
            cancellation: Option<Arc<std::sync::atomic::AtomicBool>>,
        ) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("compose-up:{}", context.project));
            if self.block_finite.load(Ordering::Acquire) {
                let flag = cancellation.expect("finite operation cancellation");
                self.finite_started.store(true, Ordering::Release);
                while !flag.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(10));
                }
                return Err(DockerError::Cancelled {
                    operation: "fixture finite workload".into(),
                });
            }
            if self.fail_compose.load(Ordering::SeqCst) {
                return Err(DockerError::Command(format!(
                    "{}\nfixture-current-compose-failure",
                    "fixture build progress\n".repeat(400)
                )));
            }
            Ok(())
        }

        fn compose_stop(&self, context: &ComposeContext) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("compose-stop:{}", context.project));
            Ok(())
        }

        fn compose_start_exact_services(
            &self,
            project: &str,
            services: &[String],
        ) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("compose-start:{project}:{}", services.join(",")));
            Ok(())
        }

        fn compose_stop_exact_services(
            &self,
            project: &str,
            services: &[String],
        ) -> Result<(), DockerError> {
            self.actions.lock().unwrap().push(format!(
                "compose-stop-exact:{project}:{}",
                services.join(",")
            ));
            Ok(())
        }

        fn compose_service_ready(
            &self,
            _project: &str,
            service: &str,
            _timeout: Duration,
        ) -> Result<(bool, String), DockerError> {
            Ok((true, format!("{service} running")))
        }

        fn compose_publishes_host_port(
            &self,
            _project: &str,
            host_port: u16,
        ) -> Result<(bool, String), DockerError> {
            Ok((
                true,
                format!("allocated host port {host_port} is published"),
            ))
        }

        fn compose_down(
            &self,
            context: &ComposeContext,
            delete_volumes: bool,
        ) -> Result<(), DockerError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("compose-down:{}:{delete_volumes}", context.project));
            Ok(())
        }

        fn compose_state(
            &self,
            _project: &str,
            services: &[String],
            finite_services: &[String],
            completions: &BTreeSet<String>,
            desired_states: &BTreeMap<String, RuntimeState>,
        ) -> Result<DockerComposeState, DockerError> {
            let finite = finite_services.iter().cloned().collect::<BTreeSet<_>>();
            let mut details = Vec::new();
            let mut candidates = Vec::new();
            for (index, service) in services.iter().enumerate() {
                if finite.contains(service) {
                    details.push(ComposeServiceState {
                        name: service.clone(),
                        role: "finite".into(),
                        state: RuntimeState::Completed,
                        desired_state: RuntimeState::Running,
                        containers: 1,
                    });
                    if !completions.contains(service) {
                        candidates.push(CompletionCandidate {
                            service: service.clone(),
                            container_id: ExactContainerId::parse(format!(
                                "{:064x}",
                                10_000 + index
                            ))?,
                            state: ContainerState {
                                state: RuntimeState::Stopped,
                                status: Some("exited".into()),
                                restarts: 0,
                                exit_code: Some(0),
                                health: None,
                                started_at: Some("start".into()),
                                finished_at: Some("finish".into()),
                                image_id: Some("sha256:finite".into()),
                                compose_service: Some(service.clone()),
                            },
                        });
                    }
                } else {
                    let state = desired_states
                        .get(service)
                        .copied()
                        .unwrap_or(RuntimeState::Running);
                    details.push(ComposeServiceState {
                        name: service.clone(),
                        role: "running".into(),
                        state,
                        desired_state: state,
                        containers: 1,
                    });
                }
            }
            let running = details
                .iter()
                .filter(|service| service.role == "running")
                .collect::<Vec<_>>();
            let state = if running.is_empty() && !details.is_empty() {
                RuntimeState::Completed
            } else if !running.is_empty()
                && running
                    .iter()
                    .all(|service| service.state == RuntimeState::Stopped)
            {
                RuntimeState::Stopped
            } else {
                RuntimeState::Running
            };
            Ok(DockerComposeState {
                state,
                containers: u32::try_from(details.len()).unwrap(),
                running: u32::try_from(
                    details
                        .iter()
                        .filter(|service| service.state == RuntimeState::Running)
                        .count(),
                )
                .unwrap(),
                services: details,
                completion_candidates: candidates,
            })
        }

        fn compose_logs(
            &self,
            context: &ComposeContext,
            _tail_lines: u16,
        ) -> Result<String, DockerError> {
            Ok(format!("compose log {}", context.project))
        }
    }

    struct FixtureGit;

    impl DeploymentGit for FixtureGit {
        fn snapshot(
            &self,
            _worktree: &Path,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<crate::deployment_git::GitSnapshot, crate::deployment_git::DeploymentGitError>
        {
            Ok(crate::deployment_git::GitSnapshot {
                commit: Some("a".repeat(40)),
                dirty: false,
                source_digest: "a".repeat(64),
            })
        }

        fn add_detached_worktree(
            &self,
            _worktree: &Path,
            _target: &Path,
            _commit: &str,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<(), crate::deployment_git::DeploymentGitError> {
            Ok(())
        }

        fn remove_worktree(
            &self,
            _worktree: &Path,
            _target: &Path,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<(), crate::deployment_git::DeploymentGitError> {
            Ok(())
        }

        fn is_ignored(
            &self,
            _worktree: &Path,
            _relative_path: &str,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<bool, crate::deployment_git::DeploymentGitError> {
            Ok(true)
        }
    }

    struct CheckoutGit {
        snapshot: Mutex<crate::deployment_git::GitSnapshot>,
        actions: Mutex<Vec<String>>,
    }

    impl CheckoutGit {
        fn new() -> Self {
            Self {
                snapshot: Mutex::new(crate::deployment_git::GitSnapshot {
                    commit: Some("a".repeat(40)),
                    dirty: false,
                    source_digest: "a".repeat(64),
                }),
                actions: Mutex::new(Vec::new()),
            }
        }
    }

    impl DeploymentGit for CheckoutGit {
        fn snapshot(
            &self,
            _worktree: &Path,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<crate::deployment_git::GitSnapshot, crate::deployment_git::DeploymentGitError>
        {
            Ok(self.snapshot.lock().unwrap().clone())
        }

        fn add_detached_worktree(
            &self,
            _worktree: &Path,
            target: &Path,
            commit: &str,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<(), crate::deployment_git::DeploymentGitError> {
            std::fs::create_dir(target).map_err(|error| {
                crate::deployment_git::DeploymentGitError::Invocation(error.to_string())
            })?;
            std::fs::write(target.join("commit"), commit).map_err(|error| {
                crate::deployment_git::DeploymentGitError::Invocation(error.to_string())
            })?;
            self.actions
                .lock()
                .unwrap()
                .push(format!("add:{}:{commit}", target.display()));
            Ok(())
        }

        fn remove_worktree(
            &self,
            _worktree: &Path,
            target: &Path,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<(), crate::deployment_git::DeploymentGitError> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("remove:{}", target.display()));
            Ok(())
        }

        fn is_ignored(
            &self,
            _worktree: &Path,
            _relative_path: &str,
            _caller_uid: u32,
            _caller_gid: u32,
        ) -> Result<bool, crate::deployment_git::DeploymentGitError> {
            Ok(true)
        }
    }

    struct FixtureHealth;

    impl DeploymentHealth for FixtureHealth {
        fn http_ready(
            &self,
            _port: u16,
            _path: &str,
            _timeout: Duration,
            _terminal: &(dyn Fn() -> Option<String> + Sync),
        ) -> Readiness {
            Readiness::ready("http 204")
        }

        fn tcp_ready(
            &self,
            _host: &str,
            _port: u16,
            _timeout: Duration,
            _terminal: &(dyn Fn() -> Option<String> + Sync),
        ) -> Readiness {
            Readiness::ready("tcp open")
        }
    }

    struct FixturePorts;

    impl crate::ports::PortAvailability for FixturePorts {
        fn bindable(&self, _port: u16) -> bool {
            true
        }
    }

    struct MutableClock(Mutex<time::OffsetDateTime>);

    impl Clock for MutableClock {
        fn now_utc(&self) -> time::OffsetDateTime {
            *self.0.lock().unwrap()
        }
    }

    struct World {
        _temporary: tempfile::TempDir,
        database: Database,
        deployments: Deployments,
        docker: Arc<FakeDocker>,
        worktree: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let temporary = tempdir().unwrap();
            let worktree = temporary.path().join("repository");
            std::fs::create_dir(&worktree).unwrap();
            std::fs::write(worktree.join(".git"), "gitdir: /missing\n").unwrap();
            std::fs::write(
                worktree.join(".devcoordinator.toml"),
                r#"
schema=2
[deployment.web]
domain="app"
components=["api","cache","stack","smtp"]
[deployment.web.component.api]
type="process"
command=["serve"]
port=true
route=true
[deployment.web.component.cache]
type="docker"
image="cache:1"
[deployment.web.component.stack]
type="compose"
services=["bootstrap","worker"]
finite_services=["bootstrap"]
independent_services=["worker"]
[deployment.web.component.smtp]
type="external"
tcp="127.0.0.1:25"
"#,
            )
            .unwrap();
            let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
            let root = worktree.display().to_string();
            database.transaction(move |transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111',?1,'repo','t',1000,'t')",[&root])?;
                transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111',?1,'t','t')",[&root])?;
                Ok(())
            }).unwrap();
            let config = Config {
                socket_path: temporary.path().join("daemon.sock"),
                state_dir: temporary.path().join("state"),
                unit_prefix: "devcoordinator2-test".into(),
                slice_name: "devcoordinator2-tests.slice".into(),
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
            let docker = Arc::new(FakeDocker::new());
            let deployments = Deployments::with_adapters(
                config,
                database.clone(),
                Registry::new(database.clone()),
                docker.clone(),
                Arc::new(FakeSystemd),
                Arc::new(ReadyNetwork),
                Arc::new(HostClock),
            );
            Self {
                _temporary: temporary,
                database,
                deployments,
                docker,
                worktree,
            }
        }

        fn caller() -> Caller {
            Caller {
                pid: 1,
                uid: 1000,
                gid: 1000,
                client_kind: devcoordinator2_api::ClientKind::Other,
                client_session: None,
                work: None,
                identity: None,
            }
        }

        fn seed_managed(&self) -> String {
            let specification = load_deployment_spec(&self.worktree, "web").unwrap();
            let deployment_id =
                DeploymentStore::deployment_id("w1111111111111111", "web", "worktree");
            self.deployments
                .store
                .upsert(
                    &deployment_id,
                    &RegisteredDeploymentTarget {
                        repository_id: "r1111111111111111".into(),
                        worktree_id: "w1111111111111111".into(),
                    },
                    &specification,
                    "worktree",
                    Some("app"),
                    "running",
                    1000,
                    "other",
                    None,
                )
                .unwrap();
            for (name, kind, identity) in [
                ("api", "unit", "api.service".to_owned()),
                ("cache", "container", "a".repeat(64)),
                ("stack", "compose", "dc2-stack".to_owned()),
            ] {
                self.deployments
                    .store
                    .set_component_runtime(
                        &deployment_id,
                        name,
                        ComponentRuntimePatch {
                            state: Some("running".into()),
                            health: Some("healthy".into()),
                            generation: Some(Some(1)),
                            binding_kind: Some(Some(kind.into())),
                            binding_identity: Some(Some(identity)),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
            self.deployments
                .store
                .set_component_runtime(
                    &deployment_id,
                    "smtp",
                    ComponentRuntimePatch {
                        state: Some("running".into()),
                        health: Some("healthy".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            self.database.transaction({let deployment_id=deployment_id.clone(); move |transaction| {
                transaction.execute("INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at) VALUES(20001,?1,'api',1,'t')",[deployment_id])?;
                Ok(())
            }}).unwrap();
            self.deployments
                .store
                .set_route(
                    Some("app"),
                    &deployment_id,
                    Some("api"),
                    Some(20001),
                    Some(1),
                )
                .unwrap();
            self.deployments
                .store
                .set_deployment_runtime(&deployment_id, "running", Some(1), None)
                .unwrap();
            self.deployments
                .store
                .record_compose_completions(
                    &deployment_id,
                    "stack",
                    1,
                    &[ComposeCompletionInput {
                        service: "bootstrap".into(),
                        container_id: "b".repeat(64),
                        image_id: None,
                        exit_code: 0,
                        started_at: None,
                        finished_at: None,
                    }],
                )
                .unwrap();
            self.docker
                .states
                .lock()
                .unwrap()
                .insert("a".repeat(64), FakeDocker::running(None));
            *self.docker.compose.lock().unwrap() = Some(DockerComposeState {
                state: RuntimeState::Running,
                containers: 2,
                running: 1,
                services: vec![
                    ComposeServiceState {
                        name: "bootstrap".into(),
                        role: "finite".into(),
                        state: RuntimeState::Completed,
                        desired_state: RuntimeState::Running,
                        containers: 1,
                    },
                    ComposeServiceState {
                        name: "worker".into(),
                        role: "running".into(),
                        state: RuntimeState::Running,
                        desired_state: RuntimeState::Running,
                        containers: 1,
                    },
                ],
                completion_candidates: Vec::new(),
            });
            deployment_id
        }
    }

    #[test]
    fn managed_status_and_list_refresh_real_runtime_shapes() {
        let world = World::new();
        let deployment_id = world.seed_managed();
        let status = world
            .deployments
            .status(None, None, Some(&deployment_id), &World::caller())
            .unwrap();
        assert_eq!(status.state, "running");
        assert_eq!(status.domain.as_deref(), Some("app"));
        assert_eq!(status.route_port, Some(20001));
        assert!(status.log_dir.is_none());
        let api = status
            .components
            .iter()
            .find(|component| component.name == "api")
            .unwrap();
        assert_eq!(api.restarts, Some(3));
        assert_eq!(api.binding.kind.as_deref(), Some("unit"));
        let stack = status
            .components
            .iter()
            .find(|component| component.name == "stack")
            .unwrap();
        assert_eq!(stack.services.as_ref().unwrap()[0].containers, 1);
        assert_eq!(
            stack.completed_services.as_ref().unwrap()[0].service,
            "bootstrap"
        );
        let list = world
            .deployments
            .list(DeploymentListParams { path: None }, &World::caller())
            .unwrap();
        assert_eq!(list.deployments.len(), 1);
        assert_eq!(list.deployments[0].deployment_id, deployment_id);
    }

    #[test]
    fn observed_control_logs_and_routes_use_only_recorded_exact_ids() {
        let world = World::new();
        let deployment_id = "d2222222222222222";
        let container_id = "c".repeat(64);
        world
            .deployments
            .store
            .replace_observed_current(
                &[ObservedDeploymentInput {
                    deployment_id: deployment_id.into(),
                    repository_id: "r1111111111111111".into(),
                    name: "native".into(),
                    native_project: "native".into(),
                    state: "running".into(),
                    health: "healthy".into(),
                    evidence: serde_json::json!({}),
                }],
                &[ObservedContainerInput {
                    container_id: container_id.clone(),
                    deployment_id: deployment_id.into(),
                    repository_id: "r1111111111111111".into(),
                    name: "native-api".into(),
                    image: "image".into(),
                    compose_service: "api".into(),
                    status: "running".into(),
                    health: "healthy".into(),
                }],
                &[],
                "2026-09-04T00:00:00Z",
            )
            .unwrap();
        world
            .docker
            .states
            .lock()
            .unwrap()
            .insert(container_id.clone(), FakeDocker::running(Some("api")));
        let status = world
            .deployments
            .control_observed("restart", deployment_id, Some("api"))
            .unwrap();
        assert_eq!(status.state, "running");
        assert_eq!(
            world.docker.actions.lock().unwrap().as_slice(),
            [format!("restart:{container_id}")]
        );
        let logs = world
            .deployments
            .observed_logs(deployment_id, "api", 20)
            .unwrap();
        assert!(logs.tail.contains(&container_id));
        let changed = world
            .deployments
            .set_domain(SetDomain {
                deployment_id: deployment_id.into(),
                domain: Some("native".into()),
                port: Some(20002),
                component: None,
                public: Some(true),
            })
            .unwrap();
        assert_eq!(
            (changed.route_port, changed.public),
            (Some(20002), Some(true))
        );
        assert!(
            world
                .deployments
                .reject_observed_configuration(deployment_id)
                .is_err()
        );
    }

    #[test]
    fn path_resolution_and_managed_domain_rules_remain_strict() {
        let world = World::new();
        let deployment_id = world.seed_managed();
        let by_name = world.deployments.status(
            Some(world.worktree.to_str().unwrap()),
            Some("web"),
            None,
            &World::caller(),
        );
        // The fixture is deliberately not a Git repository; name resolution
        // must not bypass canonical registration.
        assert_eq!(by_name.unwrap_err().code, ErrorCode::RepositoryNotFound);
        let error = world
            .deployments
            .set_domain(SetDomain {
                deployment_id: deployment_id.clone(),
                domain: Some("other".into()),
                port: Some(1),
                component: None,
                public: None,
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ParamsInvalid);
        let changed = world
            .deployments
            .set_domain(SetDomain {
                deployment_id,
                domain: Some("other".into()),
                port: None,
                component: None,
                public: Some(true),
            })
            .unwrap();
        assert_eq!(
            (changed.domain.as_deref(), changed.public),
            (Some("other"), Some(true))
        );
    }

    #[test]
    fn managed_worktree_apply_is_idempotent_and_controls_logs_and_removal() {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&worktree)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .status()
            .unwrap();
        assert!(initialized.success());
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.web]
source="worktree"
ttl_seconds=60
components=["cache"]
[deployment.web.component.cache]
type="docker"
image="cache:1"
"#,
        )
        .unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
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
        let docker = Arc::new(MutationDocker::new());
        let clock = Arc::new(MutableClock(Mutex::new(datetime!(2026-09-04 00:00 UTC))));
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database.clone()),
            docker.clone(),
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            Arc::new(FixtureGit),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            clock.clone(),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: Some("fixture".into()),
            work: None,
            identity: None,
        };
        assert_ne!(
            caller.uid, 0,
            "managed repository commands require non-root fixture uid"
        );
        let path = worktree.to_str().unwrap();
        let applied = deployments
            .apply(Some(path), Some("web"), None, &caller)
            .unwrap();
        assert_eq!(applied.state, "running");
        assert_eq!(applied.current_generation, Some(1));
        assert_eq!(applied.components[0].state, "running");
        let deployment_id = applied.deployment_id.clone();

        let unchanged = deployments
            .apply(None, None, Some(&deployment_id), &caller)
            .unwrap();
        assert_eq!(unchanged.unchanged, Some(true));
        assert_eq!(unchanged.current_generation, Some(1));

        let original_binding = unchanged.components[0].binding.identity.clone();
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.web]
source="worktree"
ttl_seconds=60
components=["cache"]
[deployment.web.component.cache]
type="docker"
image="cache:2"
"#,
        )
        .unwrap();
        docker.fail_create.store(true, Ordering::SeqCst);
        let failed = deployments
            .apply(None, None, Some(&deployment_id), &caller)
            .unwrap_err();
        assert_eq!(failed.code, ErrorCode::DeploymentApplyFailed);
        let preserved = deployments.store.get(&deployment_id).unwrap().unwrap();
        assert_eq!(preserved.current_generation, Some(1));
        assert_eq!(preserved.state, "degraded");
        assert_eq!(
            deployments.store.components(&deployment_id).unwrap()[0].binding_identity,
            original_binding
        );
        docker.fail_create.store(false, Ordering::SeqCst);
        let reapplied = deployments
            .apply(None, None, Some(&deployment_id), &caller)
            .unwrap();
        assert_eq!(reapplied.current_generation, Some(2));

        let stopped = deployments
            .control("stop", None, None, Some(&deployment_id), None, &caller)
            .unwrap();
        assert_eq!(stopped.state, "stopped");
        let started = deployments
            .control("start", None, None, Some(&deployment_id), None, &caller)
            .unwrap();
        assert_eq!(started.state, "running");
        let logs = deployments
            .logs(None, None, Some(&deployment_id), "cache", 20, &caller)
            .unwrap();
        assert!(logs.tail.starts_with("managed log "));

        let held = deployments.acquire_busy(&deployment_id).unwrap();
        let busy = deployments
            .control("restart", None, None, Some(&deployment_id), None, &caller)
            .unwrap_err();
        assert_eq!(busy.code, ErrorCode::Busy);
        drop(held);

        *clock.0.lock().unwrap() = datetime!(2026-09-04 00:02 UTC);
        let expired = deployments.expire_previews().unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].state, "stopped");

        let removed = deployments
            .remove(None, None, Some(&deployment_id), false, &caller)
            .unwrap();
        assert!(removed.removed);
        assert!(!removed.data_deleted);
        assert!(deployments.store.get(&deployment_id).unwrap().is_none());
        assert!(
            docker
                .actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| action.starts_with("remove:"))
        );
    }

    #[test]
    fn checkout_apply_retains_previous_and_rollback_keeps_the_selected_path() {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.web]
source="checkout"
components=["cache"]
[deployment.web.component.cache]
type="docker"
image="cache:1"
"#,
        )
        .unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
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
        let docker = Arc::new(MutationDocker::new());
        let git = Arc::new(CheckoutGit::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database),
            docker,
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            git.clone(),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: None,
            work: None,
            identity: None,
        };
        assert_ne!(caller.uid, 0, "checkout fixture requires a non-root caller");
        let first = deployments
            .apply(Some(worktree.to_str().unwrap()), Some("web"), None, &caller)
            .unwrap();
        let first_path = config
            .deployments_dir()
            .join(&first.deployment_id)
            .join("gen-1");
        assert!(first_path.is_dir());
        git.snapshot.lock().unwrap().commit = Some("b".repeat(40));
        let second = deployments
            .apply(None, None, Some(&first.deployment_id), &caller)
            .unwrap();
        assert_eq!(
            (second.current_generation, second.previous_generation),
            (Some(2), Some(1))
        );
        assert!(first_path.is_dir());

        let rolled = deployments
            .rollback(None, None, Some(&first.deployment_id), &caller)
            .unwrap();
        assert_eq!(rolled.rolled_back_from, Some(2));
        assert_eq!(rolled.rolled_back_to, Some(1));
        assert_eq!(rolled.current_generation, Some(3));
        assert!(first_path.is_dir());
        assert_eq!(
            std::fs::read_to_string(first_path.join("commit")).unwrap(),
            "a".repeat(40)
        );

        deployments
            .remove(None, None, Some(&first.deployment_id), false, &caller)
            .unwrap();
        assert!(!first_path.exists());
        assert!(
            git.actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| action.contains("gen-1"))
        );
    }

    #[test]
    fn process_build_output_and_failed_rebuild_preserve_the_live_generation() {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        let write_configuration = |build: &str| {
            std::fs::write(
                worktree.join(".devcoordinator.toml"),
                format!(
                    r#"
schema=2
[deployment.web]
source="worktree"
build=["{build}"]
components=["api"]
[deployment.web.component.api]
type="process"
command=["serve"]
"#
                ),
            )
            .unwrap();
        };
        write_configuration("build-one");
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
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
        let systemd = Arc::new(MutationSystemd::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database),
            Arc::new(MutationDocker::new()),
            systemd.clone(),
            Arc::new(ReadyNetwork),
            Arc::new(FixtureGit),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: None,
            work: None,
            identity: None,
        };
        assert_ne!(caller.uid, 0, "process fixture requires a non-root caller");
        let first = deployments
            .apply(Some(worktree.to_str().unwrap()), Some("web"), None, &caller)
            .unwrap();
        assert_eq!(first.current_generation, Some(1));
        assert_eq!(first.components[0].binding.kind.as_deref(), Some("unit"));
        let build_log = deployments
            .logs(None, None, Some(&first.deployment_id), "build", 20, &caller)
            .unwrap();
        assert!(build_log.tail.contains("build stdout"));
        assert!(build_log.tail.contains("build stderr"));
        let process_log = deployments
            .logs(None, None, Some(&first.deployment_id), "api", 20, &caller)
            .unwrap();
        assert_eq!(process_log.tail, "process output");

        write_configuration("build-two");
        systemd.build_exit.store(7, Ordering::SeqCst);
        let failed = deployments
            .apply(None, None, Some(&first.deployment_id), &caller)
            .unwrap_err();
        assert_eq!(failed.code, ErrorCode::DeploymentApplyFailed);
        assert_eq!(
            deployments
                .store
                .get(&first.deployment_id)
                .unwrap()
                .unwrap()
                .current_generation,
            Some(1)
        );
        let components = deployments.store.components(&first.deployment_id).unwrap();
        assert_eq!(components[0].state, "running");
        assert!(
            systemd
                .actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| action.contains("build-g2.service"))
        );
        systemd.build_exit.store(0, Ordering::SeqCst);
        let recovered = deployments
            .apply(None, None, Some(&first.deployment_id), &caller)
            .unwrap();
        assert_eq!(recovered.current_generation, Some(2));
    }

    fn finite_world() -> (
        tempfile::TempDir,
        PathBuf,
        Deployments,
        Arc<MutationDocker>,
        Caller,
    ) {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(worktree.join("compose.yml"), "services: {}\n").unwrap();
        std::fs::write(worktree.join(".devcoordinator.toml"), "schema=2\n[deployment.job]\nsource='worktree'\ncomponents=['probe']\n[deployment.job.component.probe]\ntype='compose'\nfiles=['compose.yml']\nservices=['probe']\nfinite_services=['probe']\n").unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
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
        let docker = Arc::new(MutationDocker::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database),
            docker.clone(),
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            Arc::new(FixtureGit),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: None,
            work: None,
            identity: None,
        };
        (temporary, worktree, deployments, docker, caller)
    }

    #[test]
    fn finite_workload_completes_without_running_or_repeating_a_service() {
        let (_temporary, worktree, deployments, docker, caller) = finite_world();
        let first = deployments
            .apply(Some(worktree.to_str().unwrap()), Some("job"), None, &caller)
            .unwrap();
        assert_eq!(first.state, "completed");
        assert_eq!(first.components[0].state, "completed");
        assert_eq!(first.components[0].health, "healthy");
        assert!(first.route_port.is_none());
        assert_eq!(
            first.components[0].completed_services.as_ref().unwrap()[0].exit_code,
            0
        );
        let before = docker.actions.lock().unwrap().len();
        let again = deployments
            .apply(None, None, Some(&first.deployment_id), &caller)
            .unwrap();
        assert_eq!(again.state, "completed");
        assert_eq!(again.unchanged, Some(true));
        assert_eq!(docker.actions.lock().unwrap().len(), before);
        let started = deployments
            .control(
                "start",
                None,
                None,
                Some(&first.deployment_id),
                None,
                &caller,
            )
            .unwrap();
        assert_eq!(started.state, "completed");
        assert_eq!(docker.actions.lock().unwrap().len(), before);
    }

    #[test]
    fn finite_workload_failure_is_not_completion_and_can_recover() {
        let (_temporary, worktree, deployments, docker, caller) = finite_world();
        let target = deployments
            .resolve_target(Some(worktree.to_str().unwrap()), Some("job"), None, &caller)
            .unwrap();
        docker.fail_compose.store(true, Ordering::Release);
        assert!(
            deployments
                .apply(Some(worktree.to_str().unwrap()), Some("job"), None, &caller)
                .is_err()
        );
        let row = deployments
            .store
            .get(&target.deployment_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "failed");
        assert!(
            deployments
                .store
                .compose_completions(&target.deployment_id, "probe", 1)
                .unwrap()
                .is_empty()
        );
        docker.fail_compose.store(false, Ordering::Release);
        let recovered = deployments
            .apply(None, None, Some(&target.deployment_id), &caller)
            .unwrap();
        assert_eq!(recovered.state, "completed");
    }

    #[test]
    fn finite_workload_stop_cancels_busy_apply_and_allows_recovery() {
        let (_temporary, worktree, deployments, docker, caller) = finite_world();
        let target = deployments
            .resolve_target(Some(worktree.to_str().unwrap()), Some("job"), None, &caller)
            .unwrap();
        let id = target.deployment_id;
        docker.block_finite.store(true, Ordering::Release);
        let applying = deployments.clone();
        let worker_caller = caller.clone();
        let worker = thread::spawn(move || {
            applying.apply(
                Some(worktree.to_str().unwrap()),
                Some("job"),
                None,
                &worker_caller,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !docker.finite_started.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "finite fixture did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let stopped = deployments
            .control("stop", None, None, Some(&id), None, &caller)
            .unwrap();
        assert_eq!(stopped.state, "cancelled");
        assert!(worker.join().unwrap().is_err());
        assert!(docker.actions.lock().unwrap().iter().any(|action| {
            action.starts_with("compose-stop:") || action.starts_with("compose-down:")
        }));
        docker.block_finite.store(false, Ordering::Release);
        let recovered = deployments.apply(None, None, Some(&id), &caller).unwrap();
        assert_eq!(recovered.state, "completed");
    }

    #[test]
    fn compose_finite_receipts_independent_control_logs_and_removal_are_preserved() {
        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(worktree.join("compose.yml"), "services: {}\n").unwrap();
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.web]
source="worktree"
domain="app"
components=["stack"]
[deployment.web.component.stack]
type="compose"
files=["compose.yml"]
services=["bootstrap","worker"]
finite_services=["bootstrap"]
independent_services=["worker"]
port=true
route=true
"#,
        )
        .unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
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
        let docker = Arc::new(MutationDocker::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database),
            docker.clone(),
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            Arc::new(FixtureGit),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: None,
            work: None,
            identity: None,
        };
        assert_ne!(caller.uid, 0, "Compose fixture requires a non-root caller");
        let applied = deployments
            .apply(Some(worktree.to_str().unwrap()), Some("web"), None, &caller)
            .unwrap();
        let stack = &applied.components[0];
        assert_eq!(stack.state, "running");
        assert_eq!(applied.domain.as_deref(), Some("app"));
        assert_eq!(applied.route_port, Some(40_000));
        assert_eq!(
            stack.completed_services.as_ref().unwrap()[0].service,
            "bootstrap"
        );
        assert!(
            stack
                .services
                .as_ref()
                .unwrap()
                .iter()
                .any(|service| service.name == "worker" && service.independent)
        );

        let stopped = deployments
            .control(
                "stop",
                None,
                None,
                Some(&applied.deployment_id),
                Some("stack/worker"),
                &caller,
            )
            .unwrap();
        assert_eq!(stopped.state, "stopped");
        assert_eq!(
            stopped.components[0].services.as_ref().unwrap()[1].desired_state,
            "stopped"
        );
        let started = deployments
            .control(
                "start",
                None,
                None,
                Some(&applied.deployment_id),
                Some("stack/worker"),
                &caller,
            )
            .unwrap();
        assert_eq!(started.state, "running");
        let logs = deployments
            .logs(
                None,
                None,
                Some(&applied.deployment_id),
                "stack",
                20,
                &caller,
            )
            .unwrap();
        assert!(logs.tail.contains("compose log"));
        deployments
            .store
            .set_component_runtime(
                &applied.deployment_id,
                "stack",
                ComponentRuntimePatch {
                    last_error: Some(Some("obsolete env-file denial".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        deployments
            .store
            .patch_deployment_runtime(
                &applied.deployment_id,
                DeploymentRuntimePatch {
                    state: Some("degraded".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        docker.fail_compose.store(true, Ordering::SeqCst);
        let failure = deployments
            .apply(None, None, Some(&applied.deployment_id), &caller)
            .unwrap_err();
        assert!(!failure.message.contains("fixture-current-compose-failure"));
        let logs = deployments
            .logs(
                None,
                None,
                Some(&applied.deployment_id),
                "build",
                500,
                &caller,
            )
            .unwrap();
        assert!(logs.tail.contains("fixture-current-compose-failure"));
        assert!(logs.tail.contains("fixture build progress"));
        let restored = deployments
            .store
            .components(&applied.deployment_id)
            .unwrap();
        assert_eq!(restored[0].generation, stack.generation);
        assert_eq!(restored[0].state, "running");
        assert_ne!(
            restored[0].last_error.as_deref(),
            Some("obsolete env-file denial")
        );
        assert!(
            restored[0]
                .last_error
                .as_deref()
                .unwrap()
                .contains("failed")
        );
        deployments
            .remove(None, None, Some(&applied.deployment_id), true, &caller)
            .unwrap();
        assert!(
            docker
                .actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| action.contains("compose-down") && action.ends_with("true"))
        );
    }

    #[test]
    fn postgres_credentials_health_and_explicit_data_deletion_stay_private() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", "/nonexistent")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            worktree.join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.database]
source="worktree"
components=["db"]
[deployment.database.component.db]
type="postgres"
image="postgres:18"
user="app"
database="app"
"#,
        )
        .unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let database = Database::open(state.join("authority.sqlite3")).unwrap();
        let config = Config {
            socket_path: temporary.path().join("daemon.sock"),
            state_dir: state,
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
            client_group: "clients".into(),
            port_range: (41000, 41100),
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
        let docker = Arc::new(MutationDocker::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            database.clone(),
            Registry::new(database),
            docker.clone(),
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            Arc::new(FixtureGit),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 00:00 UTC))),
        );
        let caller = Caller {
            pid: 1,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: None,
            work: None,
            identity: None,
        };
        assert_ne!(
            caller.uid, 0,
            "PostgreSQL fixture requires a non-root caller"
        );
        let applied = deployments
            .apply(
                Some(worktree.to_str().unwrap()),
                Some("database"),
                None,
                &caller,
            )
            .unwrap();
        assert_eq!(applied.state, "running");
        assert_eq!(applied.components[0].port, Some(41_000));
        let secret = config
            .secrets_dir()
            .join(&applied.deployment_id)
            .join("db.json");
        assert_eq!(
            std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let serialized = serde_json::to_string(&applied).unwrap();
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("postgresql://"));

        let removed = deployments
            .remove(None, None, Some(&applied.deployment_id), true, &caller)
            .unwrap();
        assert!(removed.data_deleted);
        assert_eq!(removed.deleted_volumes.len(), 1);
        assert!(!secret.exists());
        assert!(
            docker
                .actions
                .lock()
                .unwrap()
                .iter()
                .any(|action| action.contains("volume-remove:"))
        );
    }

    #[test]
    fn preflight_and_live_authorization_gate_all_runtime_changes_and_report_source_readiness() {
        use std::os::unix::fs::PermissionsExt;
        let world = World::new();
        let worktree = world._temporary.path().join("prerequisite-repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&worktree)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(worktree.join("compose.yml"), "services: {}\n").unwrap();
        std::fs::write(worktree.join(".gitignore"), ".worker.env\n.web.env\n").unwrap();
        for file in [".worker.env", ".web.env"] {
            std::fs::write(worktree.join(file), "FIXTURE=private-fixture-marker\n").unwrap();
        }
        let specification = r#"
schema=2
[deployment.web]
source="worktree"
components=["cache","worker","web"]
[deployment.web.component.cache]
type="docker"
image="cache:1"
[deployment.web.component.worker]
type="compose"
files=["compose.yml"]
services=["bootstrap","worker"]
finite_services=["bootstrap"]
independent_services=["worker"]
env_file=".worker.env"
[deployment.web.component.web]
type="compose"
files=["compose.yml"]
services=["web"]
env_file=".web.env"
"#;
        std::fs::write(worktree.join(".devcoordinator.toml"), specification).unwrap();
        let policy = world._temporary.path().join("compose-policy.json");
        std::fs::write(&policy, r#"{"schema":1,"authorizations":[]}"#).unwrap();
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut config = world.deployments.config().clone();
        config.compose_env_allowlist_file = Some(policy.clone());
        let docker = Arc::new(MutationDocker::new());
        let git = Arc::new(CheckoutGit::new());
        let deployments = Deployments::with_runtime_adapters(
            config.clone(),
            world.database.clone(),
            Registry::new(world.database.clone()),
            docker.clone(),
            Arc::new(FakeSystemd),
            Arc::new(ReadyNetwork),
            git.clone(),
            Arc::new(FixtureHealth),
            DeploymentFiles::new(config.deployments_dir(), config.secrets_dir()),
            Arc::new(FixturePorts),
            Arc::new(HostClock),
        );
        let caller = Caller {
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            ..World::caller()
        };
        let path = worktree.to_str().unwrap();
        let repository_count = || {
            world
                .database
                .call(|connection| {
                    Ok(
                        connection.query_row("SELECT count(*) FROM repositories", [], |row| {
                            row.get::<_, i64>(0)
                        })?,
                    )
                })
                .unwrap()
        };
        let before = repository_count();
        let preflight = deployments
            .preflight(Some(path), Some("web"), None, &caller)
            .unwrap();
        assert!(!preflight.ready);
        assert_eq!(preflight.blockers.len(), 2);
        assert!(
            preflight
                .blockers
                .iter()
                .all(|blocker| blocker.code == ErrorCode::AuthorizationRequired)
        );
        assert_eq!(repository_count(), before);
        let initial = deployments.configuration().get().unwrap();
        assert_eq!(
            deployments
                .apply(Some(path), Some("web"), None, &caller)
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationRequired
        );
        assert!(docker.actions.lock().unwrap().is_empty());
        assert_eq!(
            deployments.configuration().get().unwrap().active_revision,
            initial.active_revision
        );
        assert_eq!(
            world
                .database
                .call(|connection| Ok(connection.query_row(
                    "SELECT count(*) FROM deployments",
                    [],
                    |row| row.get::<_, i64>(0)
                )?))
                .unwrap(),
            0
        );
        let mut revision = initial.active_revision;
        for (index, file) in [".worker.env", ".web.env"].into_iter().enumerate() {
            let updated = deployments
                .set_compose_authorization(
                    devcoordinator2_api::params::SetComposeEnvAuthorization {
                        path: Some(path.into()),
                        name: Some("web".into()),
                        deployment_id: None,
                        file: file.into(),
                        authorized: true,
                        expected_revision: revision,
                    },
                    &caller,
                )
                .unwrap();
            revision = updated.active_revision;
            assert_eq!(
                deployments
                    .preflight(Some(path), Some("web"), None, &caller)
                    .unwrap()
                    .blockers
                    .len(),
                1 - index
            );
        }
        let applied = deployments
            .apply(Some(path), Some("web"), None, &caller)
            .unwrap();
        assert!(applied.readiness.as_ref().unwrap().ready);
        let receipt = applied
            .components
            .iter()
            .find(|component| component.name == "worker")
            .unwrap()
            .completed_services
            .clone()
            .unwrap();
        assert_eq!(receipt[0].generation, 1);
        let compose_up_count = || {
            docker
                .actions
                .lock()
                .unwrap()
                .iter()
                .filter(|action| action.starts_with("compose-up:"))
                .count()
        };
        let up_count = compose_up_count();
        {
            let mut source = git.snapshot.lock().unwrap();
            source.dirty = true;
            source.source_digest = "b".repeat(64);
        }
        let restarted = deployments
            .control(
                "restart",
                None,
                None,
                Some(&applied.deployment_id),
                Some("worker/worker"),
                &caller,
            )
            .unwrap();
        assert_eq!(compose_up_count(), up_count);
        assert_eq!(
            restarted
                .components
                .iter()
                .find(|component| component.name == "worker")
                .unwrap()
                .completed_services
                .as_ref()
                .unwrap(),
            &receipt
        );
        let status = deployments
            .status(None, None, Some(&applied.deployment_id), &caller)
            .unwrap();
        assert_eq!(status.state, "running");
        assert_eq!(status.readiness.as_ref().unwrap().pending_apply, Some(true));
        assert!(!status.readiness.as_ref().unwrap().ready);
        let updated = deployments
            .apply(None, None, Some(&applied.deployment_id), &caller)
            .unwrap();
        assert!(updated.readiness.as_ref().unwrap().ready);
        assert_eq!(updated.current_generation, Some(2));
        git.snapshot.lock().unwrap().source_digest = "c".repeat(64);
        assert_eq!(
            deployments
                .status(None, None, Some(&applied.deployment_id), &caller)
                .unwrap()
                .readiness
                .unwrap()
                .pending_apply,
            Some(true)
        );
        let expanded = specification.replace(
            "\"cache\",\"worker\",\"web\"",
            "\"cache\",\"worker\",\"web\",\"missing\"",
        ) + "\n[deployment.web.component.missing]\ntype=\"docker\"\nimage=\"fixture:1\"\n";
        std::fs::write(worktree.join(".devcoordinator.toml"), expanded).unwrap();
        let partial = deployments
            .status(None, None, Some(&applied.deployment_id), &caller)
            .unwrap();
        assert_eq!(partial.state, "degraded");
        assert_eq!(partial.readiness.unwrap().missing_components, ["missing"]);
        let encoded = serde_json::to_string(&deployments.configuration().get().unwrap()).unwrap();
        assert!(!encoded.contains("private-fixture-marker"));
        assert!(!encoded.contains(policy.to_str().unwrap()));
    }
}
