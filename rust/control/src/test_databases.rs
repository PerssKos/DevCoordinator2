//! Coordinator-owned, disposable analyzed templates and writable case databases.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use sha2::{Digest, Sha256};

use crate::capacity::FixtureHandler;
use crate::docker::{
    DockerControl, DockerInvocation, ExactContainerId, ManagedLabelContext, RunDetachedRequest,
};
use crate::platform::RandomSource;
use crate::repository_config::{PostgresSpec, PostgresTemplateSpec};
use crate::test_state::TestRunStore;

#[derive(Clone)]
pub(crate) struct SharedNode {
    pub scope: String,
    pub cleanup: bool,
}

#[derive(Default)]
pub(crate) struct SharedFixtures {
    pub specifications: BTreeMap<String, PostgresSpec>,
    pub nodes: BTreeMap<String, SharedNode>,
}

struct SharedDatabase {
    docker: Arc<dyn DockerControl>,
    container: Option<ExactContainerId>,
    environment: BTreeMap<String, String>,
}

impl SharedDatabase {
    fn remove(&mut self) -> Result<(), String> {
        if let Some(container) = &self.container {
            self.docker
                .remove_container(container, true)
                .map_err(|_| "shared database cleanup failed")?;
            self.container = None;
        }
        Ok(())
    }
}
impl Drop for SharedDatabase {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn shared_declarations(
    spec: &crate::repository_config::TestSpec,
    checks: &[devcoordinator2_executor_protocol::CheckPlan],
    requested: &[String],
) -> Vec<(String, PostgresSpec, Vec<String>)> {
    let mut scopes = spec.postgres_instances.clone();
    if let Some(postgres) = &spec.postgres {
        scopes.insert("default".into(), postgres.clone());
    }
    scopes
        .into_iter()
        .filter_map(|(scope, postgres)| {
            if postgres.cases_only {
                return None;
            }
            let owners = checks
                .iter()
                .filter(|check| {
                    spec.check_postgres
                        .get(&check.name)
                        .is_some_and(|value| value == &scope)
                        || spec.postgres.is_some() && scope == "default"
                })
                .map(|check| check.name.clone())
                .collect::<Vec<_>>();
            let (_, setup, cleanup) = shared_phase_ids(&scope);
            (!owners.is_empty() || requested.contains(&setup) || requested.contains(&cleanup))
                .then_some((scope, postgres, owners))
        })
        .collect()
}
fn shared_phase_ids(scope: &str) -> (String, String, String) {
    let suffix = &hash(scope.as_bytes())[..16];
    (
        format!("shared-{suffix}"),
        format!("postgres-setup-{suffix}"),
        format!("postgres-cleanup-{suffix}"),
    )
}
pub(crate) fn validate_shared_phase_names(
    spec: &crate::repository_config::TestSpec,
    checks: &[devcoordinator2_executor_protocol::CheckPlan],
    requested: &[String],
) -> Result<(), String> {
    let declarations = shared_declarations(spec, checks, requested);
    if checks.len() + declarations.len() * 2 > 256 {
        return Err("automatic database phases would exceed the 256-check plan limit".into());
    }
    for (scope, _, _) in declarations {
        let (_, setup, cleanup) = shared_phase_ids(&scope);
        if spec
            .checks
            .iter()
            .chain(checks)
            .any(|check| check.name == setup || check.name == cleanup)
        {
            return Err("automatic database phase conflicts with a declared check name".into());
        }
    }
    Ok(())
}

pub(crate) fn shared_phase_tier(
    spec: &crate::repository_config::TestSpec,
    name: &str,
) -> Option<devcoordinator2_executor_protocol::ValidationTier> {
    shared_declarations(spec, &spec.checks, &[])
        .into_iter()
        .find_map(|(scope, _, owners)| {
            let (_, setup, cleanup) = shared_phase_ids(&scope);
            if name != setup && name != cleanup {
                return None;
            }
            spec.checks
                .iter()
                .filter(|check| owners.contains(&check.name))
                .map(|check| check.tier)
                .min()
        })
}

pub(crate) fn add_shared_phases(
    plan: &mut devcoordinator2_executor_protocol::ExecutionPlan,
    spec: &crate::repository_config::TestSpec,
) -> Result<SharedFixtures, String> {
    use devcoordinator2_executor_protocol::{CheckPhase, CheckRole, CompletionMode, FailureMode};
    let mut result = SharedFixtures::default();
    validate_shared_phase_names(spec, &plan.checks, &plan.selection)?;
    for (scope, postgres, owners) in shared_declarations(spec, &plan.checks, &plan.selection) {
        let (key, setup, cleanup) = shared_phase_ids(&scope);
        let definitions = if owners.is_empty() {
            &spec.checks
        } else {
            &plan.checks
        };
        let scoped = definitions
            .iter()
            .filter(|check| {
                owners.contains(&check.name)
                    || owners.is_empty()
                        && (spec.check_postgres.get(&check.name) == Some(&scope)
                            || spec.postgres.is_some() && scope == "default")
            })
            .collect::<Vec<_>>();
        let mut prototype = scoped.first().copied().unwrap().clone();
        prototype.tier = scoped.iter().map(|check| check.tier).min().unwrap();
        prototype.source_name = None;
        prototype.role = CheckRole::Work;
        prototype.on_failure = FailureMode::Continue;
        prototype.resources.clear();
        prototype.after.clear();
        prototype.requires.clear();
        prototype.invalidates.clear();
        prototype.cwd = ".".into();
        prototype.env.clear();
        prototype.timeout_seconds = Some(120);
        prototype.completion = CompletionMode::Process;
        prototype.produces.clear();
        prototype.retained_artifacts.clear();
        prototype.diagnostic_sources.clear();
        prototype.consumes.clear();
        prototype.cacheable = false;
        prototype.cache_inputs.clear();
        prototype.discover = None;
        prototype.case_command = None;
        prototype.cases = None;
        prototype.expect_failure = false;
        prototype.expected_failure = None;
        prototype.expected_exit_code = None;
        prototype.qualification_of = None;
        prototype.fingerprint = plan.config_digest.clone();
        for (name, phase, is_cleanup) in [
            (setup.clone(), CheckPhase::Setup, false),
            (cleanup.clone(), CheckPhase::Cleanup, true),
        ] {
            let mut check = prototype.clone();
            check.name = name.clone();
            check.phase = phase;
            check.display_name = Some(if spec.targets.is_empty() {
                if is_cleanup {
                    "Database cleanup".into()
                } else {
                    "Database setup".into()
                }
            } else {
                format!(
                    "{scope} / Database {}",
                    if is_cleanup { "cleanup" } else { "setup" }
                )
            });
            check.command = Some(vec![
                plan.fixture_program
                    .clone()
                    .ok_or("database fixture program missing")?,
                "fixture".into(),
                name.clone(),
            ]);
            if is_cleanup {
                check.after = owners
                    .iter()
                    .cloned()
                    .chain(std::iter::once(setup.clone()))
                    .collect();
            }
            plan.checks.push(check);
            result.nodes.insert(
                name,
                SharedNode {
                    scope: key.clone(),
                    cleanup: is_cleanup,
                },
            );
        }
        for check in &mut plan.checks {
            if owners.contains(&check.name) {
                check.requires.push(setup.clone());
                plan.environment_files
                    .insert(check.name.clone(), format!("database-{key}.json"));
                plan.reused.remove(&check.name);
            }
        }
        result.specifications.insert(key, postgres);
    }
    Ok(result)
}

type Log = Option<Arc<Mutex<File>>>;
type Slot = Arc<Mutex<Option<Arc<Template>>>>;

pub(crate) struct FixtureRun {
    shared: SharedFixtures,
    shared_databases: Mutex<BTreeMap<String, Arc<Mutex<Option<SharedDatabase>>>>>,
    pool: Arc<DatabasePool>,
    specifications: BTreeMap<String, PostgresSpec>,
    root: PathBuf,
    current: File,
    context: ManagedLabelContext,
    gid: u32,
    closed: AtomicBool,
    leases: Mutex<BTreeMap<String, Arc<Mutex<Option<DatabaseLease>>>>>,
}

impl FixtureRun {
    fn execute_shared(
        &self,
        node: &str,
        shared: &SharedNode,
        cancelled: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let log = Arc::new(Mutex::new(
            TestRunStore
                .fixture_log(
                    &self.current,
                    &["scratch", node],
                    self.context.caller_uid,
                    self.gid,
                )
                .map_err(|_| "shared database diagnostics unavailable")?,
        ));
        let slot = self
            .shared_databases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(shared.scope.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut instance = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = (|| {
            if shared.cleanup {
                if let Some(database) = instance.as_mut() {
                    database.remove()?;
                }
                *instance = None;
                return Ok(());
            }
            if self.closed.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
                return Err("shared database setup cancelled".into());
            }
            if instance.is_none() {
                let spec = self
                    .shared
                    .specifications
                    .get(&shared.scope)
                    .ok_or("shared database specification missing")?;
                *instance = Some(self.pool.create_shared(
                    spec,
                    &self.context,
                    &shared.scope,
                    cancelled.clone(),
                    Some(log.clone()),
                )?);
            }
            if self.closed.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
                return Err("shared database setup cancelled".into());
            }
            TestRunStore
                .write_database_environment(
                    &self.current,
                    &shared.scope,
                    &instance.as_ref().unwrap().environment,
                    self.context.caller_uid,
                    self.gid,
                )
                .map_err(|_| "cannot publish private shared database environment")?;
            Ok(())
        })();
        if let Err(error) = &result {
            let _ = writeln!(
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                "{error}"
            );
        } else {
            let _ = writeln!(
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                "shared database {} completed",
                if shared.cleanup { "cleanup" } else { "setup" }
            );
        }
        drop(instance);
        TestRunStore
            .write_containers(
                &self.current,
                &self
                    .containers()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                self.context.caller_uid,
                self.gid,
            )
            .map_err(|_| "cannot retain database ownership receipt")?;
        result
    }
    pub fn new(
        pool: Arc<DatabasePool>,
        specifications: BTreeMap<String, PostgresSpec>,
        root: PathBuf,
        current: File,
        context: ManagedLabelContext,
        gid: u32,
        shared: SharedFixtures,
    ) -> Self {
        Self {
            shared,
            shared_databases: Mutex::new(BTreeMap::new()),
            pool,
            specifications,
            root,
            current,
            context,
            gid,
            closed: AtomicBool::new(false),
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn containers(&self) -> Vec<ExactContainerId> {
        let mut containers = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|slot| {
                slot.try_lock()
                    .ok()
                    .and_then(|entry| entry.as_ref().map(|lease| lease.container().clone()))
            })
            .collect::<Vec<_>>();
        containers.extend(
            self.shared_databases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter_map(|slot| {
                    slot.try_lock().ok().and_then(|value| {
                        value
                            .as_ref()
                            .and_then(|database| database.container.clone())
                    })
                }),
        );
        containers
    }

    pub fn finish(&self) -> bool {
        self.closed.store(true, Ordering::Release);
        let slots = std::mem::take(
            &mut *self
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut complete = true;
        let shared = std::mem::take(
            &mut *self
                .shared_databases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for slot in shared.values() {
            if let Some(mut database) = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                complete &= database.remove().is_ok();
            }
        }
        for (_, slot) in slots {
            if let Some(lease) = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                complete &= self.pool.release(lease, None).is_ok();
            }
        }
        self.pool.prune_idle(false);
        complete
    }
}

impl FixtureHandler for FixtureRun {
    fn execute(&self, node: &str, cancelled: Arc<AtomicBool>) -> Result<(), String> {
        if let Some(shared) = self.shared.nodes.get(node) {
            return self.execute_shared(node, shared, cancelled);
        }
        let parts = node.split('/').collect::<Vec<_>>();
        if parts.len() != 4 || !matches!(parts[1], "fixture" | "cleanup") {
            return Err("invalid fixture node".into());
        }
        let (check, stage, case, branch) = (parts[0], parts[1], parts[2], parts[3]);
        devcoordinator2_executor_core::LeafSelector::case(check, case)
            .map_err(|_| "invalid fixture case identity")?;
        let specification = self
            .specifications
            .get(check)
            .ok_or("fixture check is not registered")?;
        let template = if branch == "default" {
            PostgresTemplateSpec {
                init_sql: Vec::new(),
            }
        } else {
            specification
                .templates
                .get(branch)
                .cloned()
                .ok_or("fixture template is not declared")?
        };
        let log = Arc::new(Mutex::new(
            TestRunStore
                .fixture_log(
                    &self.current,
                    &["scratch", check, "cases", case, stage],
                    self.context.caller_uid,
                    self.gid,
                )
                .map_err(|_| "fixture diagnostics are unavailable")?,
        ));
        let key = devcoordinator2_executor_core::case_environment_key(check, case);
        let slot = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut entry = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = (|| {
            if stage == "cleanup" {
                if let Some(lease) = entry.take() {
                    self.pool.release(lease, Some(log.clone()))?;
                }
                return Ok(());
            }
            if self.closed.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
                return Err("fixture run is ending".into());
            }
            if entry.is_none() {
                let inputs = read_sql_inputs(&self.root, &template)?;
                let identity = format!(
                    "{}:{key}",
                    self.context
                        .run_id
                        .as_deref()
                        .ok_or("fixture run identity is missing")?
                );
                let lease = self.pool.prepare(
                    specification,
                    &inputs,
                    &self.context,
                    &identity,
                    cancelled.clone(),
                    Some(log.clone()),
                )?;
                if self.closed.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) {
                    self.pool.release(lease, Some(log.clone()))?;
                    return Err("fixture preparation cancelled".into());
                }
                *entry = Some(lease);
            }
            let lease = entry.as_ref().unwrap();
            TestRunStore
                .write_database_environment(
                    &self.current,
                    &key,
                    &lease.environment(),
                    self.context.caller_uid,
                    self.gid,
                )
                .map_err(|_| "cannot publish private fixture environment")?;
            writeln!(
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                "{}",
                serde_json::json!({"template_reused":lease.reused,"fingerprint":lease.fingerprint})
            )
            .map_err(|_| "cannot retain fixture receipt")?;
            Ok(())
        })();
        if let Err(error) = &result {
            let _ = writeln!(
                log.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                "{error}"
            );
        }
        result
    }
}

pub(crate) struct DatabasePool {
    docker: Arc<dyn DockerControl>,
    random: Arc<dyn RandomSource>,
    templates: Mutex<BTreeMap<String, Slot>>,
    owned: Mutex<Vec<Weak<Template>>>,
}

struct Template {
    docker: Arc<dyn DockerControl>,
    container: ExactContainerId,
    manager: String,
    name: String,
    user: String,
    password: String,
    port: u16,
    last_used: Mutex<Instant>,
}

impl Drop for Template {
    fn drop(&mut self) {
        let _ = self.docker.remove_container(&self.container, true);
    }
}

pub(crate) struct DatabaseLease {
    template: Arc<Template>,
    database: String,
    pub fingerprint: String,
    pub reused: bool,
}

impl DatabaseLease {
    pub fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PGHOST".into(), "127.0.0.1".into()),
            ("PGPORT".into(), self.template.port.to_string()),
            ("PGUSER".into(), self.template.user.clone()),
            ("PGPASSWORD".into(), self.template.password.clone()),
            ("PGDATABASE".into(), self.database.clone()),
            (
                "DATABASE_URL".into(),
                format!(
                    "postgresql://{}:{}@127.0.0.1:{}/{}",
                    self.template.user, self.template.password, self.template.port, self.database
                ),
            ),
        ])
    }
    pub fn container(&self) -> &ExactContainerId {
        &self.template.container
    }
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn template_fingerprint(
    repository: &str,
    image: &str,
    user: &str,
    inputs: &[(String, Vec<u8>)],
) -> String {
    let receipts = inputs
        .iter()
        .map(|(path, bytes)| (path, hash(bytes)))
        .collect::<Vec<_>>();
    hash(
        &serde_json::to_vec(&(1, repository, image, user, receipts))
            .expect("template receipt serialization"),
    )
}

pub(crate) fn read_sql_inputs(
    root: &Path,
    template: &PostgresTemplateSpec,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
    let root = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| "database input root is unavailable")?;
    let mut inputs = Vec::new();
    for name in &template.init_sql {
        let mut directory = root
            .try_clone()
            .map_err(|_| "cannot inspect database inputs")?;
        let components = Path::new(name).components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(part) = component else {
                return Err("database SQL path must be normalized and relative".into());
            };
            let last = index + 1 == components.len();
            let flags = OFlags::RDONLY
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | if last {
                    OFlags::empty()
                } else {
                    OFlags::DIRECTORY
                };
            directory = openat(&directory, *part, flags, Mode::empty())
                .map_err(|_| "database SQL input is unavailable or contains a link")?;
        }
        let stat = fstat(&directory).map_err(|_| "cannot inspect database SQL input")?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_size > 2 * 1024 * 1024
        {
            return Err("database SQL input must be a regular file up to 2 MiB".into());
        }
        let mut input = Vec::new();
        let mut file = File::from(directory);
        (&mut file)
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut input)
            .map_err(|_| "cannot read database SQL input")?;
        let after = fstat(&file).map_err(|_| "cannot recheck database SQL input")?;
        if input.len() > 2 * 1024 * 1024
            || stat.st_size != input.len() as i64
            || stat.st_size != after.st_size
            || stat.st_mtime != after.st_mtime
            || stat.st_mtime_nsec != after.st_mtime_nsec
            || stat.st_ctime != after.st_ctime
            || stat.st_ctime_nsec != after.st_ctime_nsec
        {
            return Err("database SQL input changed while it was read".into());
        }
        inputs.push((name.clone(), input));
    }
    Ok(inputs)
}

impl DatabasePool {
    fn create_shared(
        &self,
        spec: &PostgresSpec,
        context: &ManagedLabelContext,
        scope: &str,
        cancelled: Arc<AtomicBool>,
        log: Log,
    ) -> Result<SharedDatabase, String> {
        if cancelled.load(Ordering::Acquire) {
            return Err("shared database setup cancelled".into());
        }
        if !self.docker.available() {
            return Err("Docker is unavailable for the shared database".into());
        }
        if spec.image.contains("@sha256:") {
            self.docker.ensure_digest_image(&spec.image)
        } else {
            self.docker.ensure_image(&spec.image)
        }
        .map_err(|_| "shared database image is unavailable")?;
        let password = self.secret()?;
        let mut labels = context.clone();
        labels.component = Some(scope.into());
        let run_id = context
            .run_id
            .as_deref()
            .ok_or("shared database run identity missing")?;
        let container = self
            .docker
            .run_detached(&RunDetachedRequest {
                name: format!(
                    "devcoordinator2-test-{}-postgres-{scope}",
                    run_id.trim_start_matches('t')
                ),
                image: spec.image.clone(),
                label_context: labels,
                labels: BTreeMap::new(),
                env_names: vec![
                    "POSTGRES_USER".into(),
                    "POSTGRES_PASSWORD".into(),
                    "POSTGRES_DB".into(),
                    "PGDATA".into(),
                ],
                env_values: BTreeMap::from([
                    ("POSTGRES_USER".into(), spec.user.clone()),
                    ("POSTGRES_PASSWORD".into(), password.clone()),
                    ("POSTGRES_DB".into(), spec.database.clone()),
                    ("PGDATA".into(), "/var/lib/postgresql/data".into()),
                ]),
                publish: vec!["127.0.0.1::5432".into()],
                tmpfs: vec!["/var/lib/postgresql/data:rw,size=1g,mode=0700".into()],
                command: Vec::new(),
            })
            .map_err(|_| "cannot start owned shared database")?;
        let mut database = SharedDatabase {
            docker: self.docker.clone(),
            container: Some(container.clone()),
            environment: BTreeMap::new(),
        };
        let port = self
            .docker
            .published_host_port(&container, "5432/tcp")
            .map_err(|_| "shared database port unavailable")?;
        if self
            .docker
            .wait_postgres_ready_cancellable(
                &container,
                &spec.user,
                &spec.database,
                Duration::from_secs(90),
                Some(cancelled.clone()),
            )
            .is_err()
        {
            if let Ok(invocation) = DockerInvocation::new(
                vec!["logs".into(), container.as_str().into()],
                Duration::from_secs(30),
            ) {
                let _ = self.docker.invoke(invocation.with_output_log(log));
            }
            return Err("shared database did not become ready; inspect setup diagnostics".into());
        }
        if cancelled.load(Ordering::Acquire) {
            return Err("shared database setup cancelled".into());
        }
        database.environment = BTreeMap::from([
            ("PGHOST".into(), "127.0.0.1".into()),
            ("PGPORT".into(), port.to_string()),
            ("PGUSER".into(), spec.user.clone()),
            ("PGPASSWORD".into(), password.clone()),
            ("PGDATABASE".into(), spec.database.clone()),
            (
                "DATABASE_URL".into(),
                format!(
                    "postgresql://{}:{}@127.0.0.1:{port}/{}",
                    spec.user, password, spec.database
                ),
            ),
        ]);
        Ok(database)
    }
    pub fn new(docker: Arc<dyn DockerControl>, random: Arc<dyn RandomSource>) -> Self {
        Self {
            docker,
            random,
            templates: Mutex::new(BTreeMap::new()),
            owned: Mutex::new(Vec::new()),
        }
    }

    fn secret(&self) -> Result<String, String> {
        let mut bytes = [0u8; 24];
        self.random
            .fill(&mut bytes)
            .map_err(|_| "cannot create disposable database credentials")?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        specification: &PostgresSpec,
        inputs: &[(String, Vec<u8>)],
        context: &ManagedLabelContext,
        identity: &str,
        cancelled: Arc<AtomicBool>,
        log: Log,
    ) -> Result<DatabaseLease, String> {
        if cancelled.load(Ordering::Acquire) {
            return Err("database preparation cancelled".into());
        }
        if specification.image.contains("@sha256:") {
            self.docker.ensure_digest_image(&specification.image)
        } else {
            self.docker.ensure_image(&specification.image)
        }
        .map_err(|_| "database image is unavailable")?;
        let image = self
            .docker
            .image_identity(&specification.image)
            .map_err(|_| "database image identity is unavailable")?;
        let fingerprint =
            template_fingerprint(&context.repository_id, &image, &specification.user, inputs);
        let slot = self
            .templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(fingerprint.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut entry = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reused = entry
            .as_ref()
            .is_some_and(|template| self.valid(template, cancelled.clone()));
        if !reused {
            *entry = None;
            *entry = Some(Arc::new(self.create(
                specification,
                inputs,
                context,
                &fingerprint,
                cancelled.clone(),
                log.clone(),
            )?));
            self.owned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::downgrade(entry.as_ref().unwrap()));
        }
        let template = entry.as_ref().unwrap().clone();
        drop(entry);
        if cancelled.load(Ordering::Acquire) {
            return Err("database preparation cancelled".into());
        }
        let database = format!("dc2_case_{}", &hash(identity.as_bytes())[..32]);
        let cloned = self.sql(
            &template,
            "postgres",
            format!(
                "CREATE DATABASE {} OWNER {} TEMPLATE {};",
                ident(&database),
                ident(&template.user),
                ident(&template.name)
            )
            .as_bytes(),
            cancelled.clone(),
            log.clone(),
        );
        *template
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        let lease = DatabaseLease {
            template,
            database,
            fingerprint,
            reused,
        };
        if let Err(error) = cloned {
            // A disconnected client cannot tell whether CREATE committed.
            // Always reconcile its exact database, and retire an uncertain template.
            self.release(lease, log)?;
            return Err(error);
        }
        if cancelled.load(Ordering::Acquire) {
            self.release(lease, log)?;
            return Err("database preparation cancelled".into());
        }
        Ok(lease)
    }

    pub fn release(&self, lease: DatabaseLease, log: Log) -> Result<(), String> {
        let result = self.sql(
            &lease.template,
            "postgres",
            format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE);",
                ident(&lease.database)
            )
            .as_bytes(),
            Arc::new(AtomicBool::new(false)),
            log,
        );
        if result.is_err()
            && let Some(slot) = self
                .templates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&lease.fingerprint)
                .cloned()
        {
            let mut entry = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entry
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, &lease.template))
            {
                *entry = None;
            }
        }
        result
    }

    pub fn prune_idle(&self, force: bool) -> bool {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| entry.strong_count() > 0);
        let mut slots = self
            .templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        slots.sort_by_key(|slot| {
            slot.try_lock()
                .ok()
                .and_then(|entry| {
                    entry.as_ref().map(|value| {
                        value
                            .last_used
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .elapsed()
                    })
                })
                .unwrap_or_default()
        });
        let mut removed = false;
        for (index, slot) in slots.into_iter().enumerate() {
            if let Ok(mut entry) = slot.try_lock()
                && entry.as_ref().is_some_and(|value| {
                    Arc::strong_count(value) == 1
                        && (force
                            || index >= 16
                            || value
                                .last_used
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .elapsed()
                                > Duration::from_secs(3600))
                })
            {
                *entry = None;
                removed = true;
            }
        }
        self.templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, slot| {
                Arc::strong_count(slot) > 1 || slot.try_lock().map_or(true, |entry| entry.is_some())
            });
        removed
    }

    pub fn owns(&self, container: &ExactContainerId) -> bool {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(Weak::upgrade)
            .any(|entry| &entry.container == container)
    }

    fn valid(&self, template: &Template, cancelled: Arc<AtomicBool>) -> bool {
        let query = format!(
            "SELECT datistemplate AND NOT datallowconn AND pg_get_userbyid(datdba)={} FROM pg_database WHERE datname={};",
            literal(&template.manager),
            literal(&template.name)
        );
        self.query(template, "postgres", query.as_bytes(), cancelled, None)
            .is_ok_and(|output| output.stdout.trim() == "t")
    }

    fn create(
        &self,
        specification: &PostgresSpec,
        inputs: &[(String, Vec<u8>)],
        context: &ManagedLabelContext,
        fingerprint: &str,
        cancelled: Arc<AtomicBool>,
        log: Log,
    ) -> Result<Template, String> {
        let password = self.secret()?;
        let manager = format!("dc2_owner_{}", &fingerprint[..16]);
        let seed = format!("dc2_seed_{}", &fingerprint[..16]);
        let mut labels = context.clone();
        labels.run_id = None;
        labels.component = Some("database-template".into());
        let nonce = self.secret()?;
        let container = self
            .docker
            .run_detached(&RunDetachedRequest {
                name: format!(
                    "devcoordinator2-template-{}-{}",
                    &fingerprint[..16],
                    &nonce[..8]
                ),
                image: specification.image.clone(),
                label_context: labels,
                labels: BTreeMap::new(),
                env_names: vec![
                    "POSTGRES_USER".into(),
                    "POSTGRES_PASSWORD".into(),
                    "POSTGRES_DB".into(),
                    "PGDATA".into(),
                ],
                env_values: BTreeMap::from([
                    ("POSTGRES_USER".into(), manager.clone()),
                    ("POSTGRES_PASSWORD".into(), self.secret()?),
                    ("POSTGRES_DB".into(), "postgres".into()),
                    ("PGDATA".into(), "/var/lib/postgresql/data".into()),
                ]),
                publish: vec!["127.0.0.1::5432".into()],
                tmpfs: vec!["/var/lib/postgresql/data:rw,size=1g,mode=0700".into()],
                command: Vec::new(),
            })
            .map_err(|_| "cannot start owned database template")?;
        let mut template = Template {
            docker: self.docker.clone(),
            container,
            manager,
            name: format!("dc2_template_{}", &fingerprint[..24]),
            user: specification.user.clone(),
            password,
            port: 0,
            last_used: Mutex::new(Instant::now()),
        };
        template.port = self
            .docker
            .published_host_port(&template.container, "5432/tcp")
            .map_err(|_| "database template port is unavailable")?;
        self.docker
            .wait_postgres_ready_cancellable(
                &template.container,
                &template.manager,
                "postgres",
                Duration::from_secs(90),
                Some(cancelled.clone()),
            )
            .map_err(|_| "database template did not become ready")?;
        if cancelled.load(Ordering::Acquire) {
            return Err("database preparation cancelled".into());
        }
        self.sql(
            &template,
            "postgres",
            format!(
                "CREATE ROLE {} LOGIN PASSWORD {}; CREATE ROLE {} NOLOGIN SUPERUSER;",
                ident(&template.user),
                literal(&template.password),
                ident(&seed)
            )
            .as_bytes(),
            cancelled.clone(),
            None,
        )?;
        self.sql(
            &template,
            "postgres",
            format!(
                "CREATE DATABASE {} OWNER {};",
                ident(&template.name),
                ident(&template.manager)
            )
            .as_bytes(),
            cancelled.clone(),
            log.clone(),
        )?;
        for (_, input) in inputs {
            let mut sql = format!("SET ROLE {};\n", ident(&seed)).into_bytes();
            sql.extend_from_slice(input);
            self.sql(
                &template,
                &template.name,
                &sql,
                cancelled.clone(),
                log.clone(),
            )?;
        }
        self.sql(
            &template,
            &template.name,
            b"ANALYZE;",
            cancelled.clone(),
            log.clone(),
        )?;
        self.sql(&template,"postgres",format!("ALTER ROLE {} NOSUPERUSER NOCREATEDB NOCREATEROLE NOLOGIN; GRANT {} TO {}; ALTER DATABASE {} IS_TEMPLATE true; ALTER DATABASE {} ALLOW_CONNECTIONS false;",ident(&seed),ident(&seed),ident(&template.user),ident(&template.name),ident(&template.name)).as_bytes(),cancelled,log)?;
        Ok(template)
    }

    fn sql(
        &self,
        template: &Template,
        database: &str,
        sql: &[u8],
        cancelled: Arc<AtomicBool>,
        log: Log,
    ) -> Result<(), String> {
        self.query(template, database, sql, cancelled, log)
            .map(|_| ())
    }

    fn query(
        &self,
        template: &Template,
        database: &str,
        sql: &[u8],
        cancelled: Arc<AtomicBool>,
        log: Log,
    ) -> Result<crate::docker::DockerOutput, String> {
        let mut input =
            tempfile::tempfile().map_err(|_| "cannot prepare private database input")?;
        input
            .write_all(sql)
            .and_then(|()| input.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(|_| "cannot write private database input")?;
        let invocation = DockerInvocation::new(
            vec![
                "exec".into(),
                "-i".into(),
                template.container.as_str().into(),
                "psql".into(),
                "--no-psqlrc".into(),
                "--set=ON_ERROR_STOP=1".into(),
                "-tA".into(),
                "--username".into(),
                template.manager.clone().into(),
                "--dbname".into(),
                database.into(),
            ],
            Duration::from_secs(600),
        )
        .map_err(|_| "invalid database operation")?
        .with_input(input)
        .with_cancellation(Some(cancelled))
        .with_output_log(log);
        let result = self
            .docker
            .invoke(invocation)
            .map_err(|_| "database operation failed; inspect fixture diagnostics")?;
        if !result.success() {
            return Err("database operation failed; inspect fixture diagnostics".into());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_phase_names_and_limits_are_checked_before_replacing_a_run() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".devcoordinator.toml"),"schema=2\n[test.unit]\n[test.unit.postgres]\nimage='postgres:16-alpine'\n[[test.unit.check]]\nname='body'\ntier='release'\ncommand=['true']\n").unwrap();
        let spec = crate::repository_config::load_test_spec(root.path(), Some("unit")).unwrap();
        let mut checks = spec.checks.clone();
        validate_shared_phase_names(&spec, &checks, &[]).unwrap();
        checks[0].name = shared_phase_ids("default").1;
        assert!(
            validate_shared_phase_names(&spec, &checks, &[])
                .unwrap_err()
                .contains("conflicts")
        );
        let mut checks = (0..254)
            .map(|index| {
                let mut check = spec.checks[0].clone();
                check.name = format!("case-{index}");
                check
            })
            .collect::<Vec<_>>();
        validate_shared_phase_names(&spec, &checks, &[]).unwrap();
        checks.push(spec.checks[0].clone());
        assert!(
            validate_shared_phase_names(&spec, &checks, &[])
                .unwrap_err()
                .contains("256")
        );
    }
    #[test]
    fn template_fingerprint_binds_repository_image_user_paths_bytes_and_order() {
        let inputs = vec![
            ("schema.sql".into(), b"CREATE TABLE t(x int);".to_vec()),
            ("seed.sql".into(), b"INSERT INTO t VALUES(1);".to_vec()),
        ];
        let key = template_fingerprint("repo", "sha256:image1", "app", &inputs);
        assert_eq!(
            key,
            template_fingerprint("repo", "sha256:image1", "app", &inputs)
        );
        for (repository, image, user) in [
            ("other", "sha256:image1", "app"),
            ("repo", "sha256:image2", "app"),
            ("repo", "sha256:image1", "other"),
        ] {
            assert_ne!(key, template_fingerprint(repository, image, user, &inputs));
        }
        let mut changed = inputs.clone();
        changed[0].0 = "new-schema.sql".into();
        assert_ne!(
            key,
            template_fingerprint("repo", "sha256:image1", "app", &changed)
        );
        changed = inputs.clone();
        changed[0].1.push(b' ');
        assert_ne!(
            key,
            template_fingerprint("repo", "sha256:image1", "app", &changed)
        );
        changed = inputs.clone();
        changed.reverse();
        assert_ne!(
            key,
            template_fingerprint("repo", "sha256:image1", "app", &changed)
        );
        assert_ne!(
            key,
            template_fingerprint("repo", "sha256:image1", "app", &inputs[..1])
        );
    }

    #[test]
    fn sql_inputs_reject_missing_oversized_and_linked_files() {
        let root = tempfile::tempdir().unwrap();
        let spec = |name: &str| PostgresTemplateSpec {
            init_sql: vec![name.into()],
        };
        std::fs::write(root.path().join("seed.sql"), b"SELECT 1;").unwrap();
        assert_eq!(
            read_sql_inputs(root.path(), &spec("seed.sql")).unwrap()[0].1,
            b"SELECT 1;"
        );
        assert!(read_sql_inputs(root.path(), &spec("missing.sql")).is_err());
        std::os::unix::fs::symlink("seed.sql", root.path().join("linked.sql")).unwrap();
        assert!(read_sql_inputs(root.path(), &spec("linked.sql")).is_err());
        assert!(read_sql_inputs(root.path(), &spec("../seed.sql")).is_err());
        File::create(root.path().join("large.sql"))
            .unwrap()
            .set_len(2 * 1024 * 1024 + 1)
            .unwrap();
        assert!(read_sql_inputs(root.path(), &spec("large.sql")).is_err());
    }
}
