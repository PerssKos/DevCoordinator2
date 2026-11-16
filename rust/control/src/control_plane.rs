//! Composed protocol-v3 operation dispatcher.
//!
//! Domain modules stay reusable and transport-free. This layer performs the
//! one authorization decision, decodes the registry's typed input, invokes the
//! owning service, filters public collections, and serializes the typed result.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::database::OptionalExtension;
use devcoordinator2_api::params;
use devcoordinator2_api::results;
use devcoordinator2_api::{ErrorCode, OperationId, PingData, ProtocolError};
use devcoordinator2_executor_core::log_query::LogQueryOperation;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use time::{format_description::FormatItem, macros::format_description};

use crate::access::{Access, Caller, RoutePublisher};
use crate::alerts::AlertEvent;
use crate::bugs;
use crate::capacity::CapacityBroker;
use crate::config::Config;
use crate::daemon::OperationExecutor;
use crate::database::{Database, DatabaseError};
use crate::deployments::Deployments;
use crate::events::{EventService, NewEvent};
use crate::glossary::GlossaryService;
use crate::health::HealthService;
use crate::plan::{PlanService, PostgresDeploymentEvidence};
use crate::platform::{Clock, HostClock};
use crate::progress::ProgressService;
use crate::repository::Registry;
use crate::request_deadline::{WaitContext, parse_deadline, wait_deadline_error};
use crate::routes::RouteFilePublisher;
use crate::server::ServerService;
use crate::sketches::SketchService;
use crate::telegram::{TelegramEvent, TelegramScope, TelegramService, parse_scope};
use crate::test_artifacts::TestArtifactService;
use crate::test_containers::TestContainerService;
use crate::test_evidence::TestEvidenceService;
use crate::test_lifecycle::{TestLifecycle, TestLifecycleEvent};
use crate::test_logs::TestLogService;
use crate::test_state::ActiveTestArchiveBlocker;
use crate::usage::UsageService;
use crate::{DATABASE_SCHEMA_VERSION, SOURCE_COMMIT};

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone)]
pub struct ControlPlane {
    config: Arc<Config>,
    database: Database,
    access: Access,
    registry: Registry,
    plan: PlanService,
    reviews: crate::review::ReviewService,
    deliveries: crate::delivery::DeliveryService,
    glossary: GlossaryService,
    logs: TestLogService,
    tests: TestLifecycle,
    test_containers: TestContainerService,
    artifacts: TestArtifactService,
    test_evidence: TestEvidenceService,
    sketches: SketchService,
    storage: crate::storage::StorageService,
    tickets: Option<crate::ticket_service::TicketService>,
    deployments: Deployments,
    servers: ServerService,
    events: EventService,
    capacity: CapacityBroker,
    health: HealthService,
    incidents: crate::incidents::IncidentService,
    usage: UsageService,
    rate_cards: crate::rate_card::RateCardService,
    progress: ProgressService,
    telegram: TelegramService,
    agent_routing: crate::agent_routing::AgentRoutingService,
    agent_hub: crate::agent_hub::AgentHubService,
    clock: Arc<dyn Clock>,
}

impl ControlPlane {
    fn ticket_service(&self) -> Result<&crate::ticket_service::TicketService, ProtocolError> {
        self.tickets.as_ref().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ConfigurationInvalid,
                "ticket service requires DEVCOORDINATOR2_BASE_DOMAIN",
            )
        })
    }

    pub fn storage(&self) -> &crate::storage::StorageService {
        &self.storage
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn deployments(&self) -> &Deployments {
        &self.deployments
    }

    pub fn new(config: Config, database: Database) -> Result<Self, ProtocolError> {
        let publisher = Arc::new(RouteFilePublisher::new(
            database.clone(),
            config.routes_path(),
            config.base_domain.clone(),
        ));
        Self::with_adapters(config, database, publisher, Arc::new(HostClock))
    }

    pub fn with_adapters(
        config: Config,
        database: Database,
        publisher: Arc<dyn RoutePublisher>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ProtocolError> {
        let access = Access::new(&config, database.clone(), publisher)?;
        let events = EventService::new(database.clone())?;
        let registry = Registry::with_archive_blockers(database.clone(), ActiveTestArchiveBlocker);
        let capacity = CapacityBroker::new(database.clone(), config.capacity_socket_path())?;
        let deployments = Deployments::with_clock(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::clone(&clock),
        )
        .with_network_capacity(capacity.clone());
        let servers = ServerService::new(database.clone());
        let evidence = Arc::new(PostgresDeploymentEvidence::new(
            database.clone(),
            config.base_domain.clone(),
        ));
        let plan = PlanService::new(database.clone(), evidence);
        let glossary = GlossaryService::new(database.clone());
        let logs = TestLogService::new(database.clone(), registry.clone());
        let artifacts = TestArtifactService::new(database.clone(), registry.clone());
        let test_evidence =
            TestEvidenceService::with_clock(database.clone(), registry.clone(), Arc::clone(&clock));
        let sketches = SketchService::with_clock(&config, database.clone(), Arc::clone(&clock));
        // Public ticket identity needs a domain; the headless daemon does not.
        let tickets = if config.base_domain.trim().is_empty() {
            None
        } else {
            Some(crate::ticket_service::TicketService::new(
                database.clone(),
                &config.base_domain,
            )?)
        };
        let incidents =
            crate::incidents::IncidentService::new(database.clone(), Arc::clone(&clock));
        let storage = crate::storage::StorageService::new(
            config.clone(),
            database.clone(),
            Arc::clone(&clock),
            capacity.clone(),
            events.clone(),
        );
        let health = HealthService::with_adapters(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::new(crate::docker::DockerCli::default().with_network_capacity(capacity.clone())),
            Arc::new(crate::systemd::SystemdCli::default()),
            Arc::clone(&clock),
        );
        let usage = UsageService::with_clock(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::clone(&clock),
        );
        let rate_cards = crate::rate_card::RateCardService::new(database.clone());
        let progress = ProgressService::with_clock(
            database.clone(),
            registry.clone(),
            usage.usage().clone(),
            Arc::clone(&clock),
        );
        let reviews = crate::review::ReviewService::new(database.clone(), usage.clone());
        let deliveries = crate::delivery::DeliveryService::new(
            database.clone(),
            artifacts.clone(),
            config.base_domain.clone(),
            crate::deployment_files::DeploymentFiles::new(
                config.deployments_dir(),
                config.secrets_dir(),
            ),
        )
        .with_native_console(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            SOURCE_COMMIT.to_owned(),
        );
        let tests = TestLifecycle::new(
            config.clone(),
            database.clone(),
            registry.clone(),
            capacity.clone(),
            logs.clone(),
            Arc::clone(&clock),
        )?;
        let test_containers = TestContainerService::new(
            &config,
            database.clone(),
            tests.docker_control(),
            Arc::clone(&clock),
        );
        let telegram = TelegramService::from_config(&config, database.clone());
        let agent_routing = crate::agent_routing::AgentRoutingService::new(database.clone())?;
        let agent_hub = crate::agent_hub::AgentHubService::new(database.clone())?;
        let alert_telegram = telegram.clone();
        let alert_events = events.clone();
        let alert_clock = Arc::clone(&clock);
        health
            .sampler()
            .alerts()
            .set_event_sink(Arc::new(move |event: AlertEvent| {
                let _ = alert_events.publish(NewEvent {
                    occurred_at: event_timestamp(alert_clock.as_ref()),
                    dedupe_key: None,
                    event: owned_alert_event(&event),
                });
                let notification = TelegramEvent::new(event.kind)
                    .with("alert_key", event.alert_key)
                    .with("alert_kind", event.alert_kind)
                    .with("subject_kind", event.subject_kind)
                    .with("subject_id", event.subject_id)
                    .with("severity", event.severity)
                    .with("message", event.message);
                let _ = alert_telegram.enqueue_event(&notification);
            }));
        let deployment_events = events.clone();
        let deployment_clock = Arc::clone(&clock);
        deployments.set_event_sink(Arc::new(move |event| {
            deployment_events
                .publish(NewEvent {
                    occurred_at: event_timestamp(deployment_clock.as_ref()),
                    dedupe_key: None,
                    event: results::OwnedEvent::Deployment(event),
                })
                .map(|_| ())
        }));
        let test_telegram = telegram.clone();
        let test_sampler = health.sampler().clone();
        let test_events = events.clone();
        let test_clock = Arc::clone(&clock);
        tests.set_event_sink(Arc::new(move |event: TestLifecycleEvent| {
            test_sampler.request_storage();
            let _ = test_events.publish(NewEvent {
                occurred_at: event_timestamp(test_clock.as_ref()),
                dedupe_key: Some(format!("test:{}:{}", event.run_id, event.kind)),
                event: results::OwnedEvent::Test(results::TestOwnedEvent {
                    kind: event.kind.to_owned(),
                    run_id: event.run_id.clone(),
                    test: event.test.clone(),
                    status: event.status.clone(),
                    exit_code: event.exit_code,
                    repository_id: event.repository_id.clone(),
                    worktree_id: event.worktree_id.clone(),
                    duration_seconds: event.duration_seconds,
                }),
            });
            let status = serde_json::to_value(event.status).unwrap_or(Value::Null);
            let notification = TelegramEvent::new(event.kind)
                .with("run_id", event.run_id)
                .with("test", event.test)
                .with("status", status)
                .with("exit_code", event.exit_code)
                .with("repository_id", event.repository_id)
                .with("worktree_id", event.worktree_id)
                .with("duration_seconds", event.duration_seconds)
                .with("caller_uid", event.caller_uid)
                .with("client", event.client);
            let _ = test_telegram.enqueue_event(&notification);
        }));
        Ok(Self {
            config: Arc::new(config),
            database,
            access,
            registry,
            plan,
            reviews,
            deliveries,
            glossary,
            logs,
            tests,
            test_containers,
            artifacts,
            test_evidence,
            sketches,
            storage,
            tickets,
            deployments,
            servers,
            events,
            capacity,
            health,
            incidents,
            usage,
            rate_cards,
            progress,
            telegram,
            agent_routing,
            agent_hub,
            clock,
        })
    }

    pub fn access(&self) -> &Access {
        &self.access
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn capacity(&self) -> &CapacityBroker {
        &self.capacity
    }

    pub fn health(&self) -> &HealthService {
        &self.health
    }

    pub fn usage(&self) -> &UsageService {
        &self.usage
    }

    pub fn progress(&self) -> &ProgressService {
        &self.progress
    }

    pub fn logs(&self) -> &TestLogService {
        &self.logs
    }

    pub fn tests(&self) -> &TestLifecycle {
        &self.tests
    }

    pub fn test_evidence(&self) -> &TestEvidenceService {
        &self.test_evidence
    }

    pub fn events(&self) -> &EventService {
        &self.events
    }

    pub fn recover_tests(&self) -> Result<(), ProtocolError> {
        self.tests.recover()
    }

    pub fn reconcile_routes(&self) -> Result<(), ProtocolError> {
        self.deployments.reconcile_routes()
    }

    pub fn telegram(&self) -> &TelegramService {
        &self.telegram
    }

    pub fn agent_hub(&self) -> &crate::agent_hub::AgentHubService {
        &self.agent_hub
    }

    pub fn deliver_hub_callbacks(&self) -> Result<u32, ProtocolError> {
        self.agent_hub.deliver_once()
    }

    pub fn expire_previews(&self) -> Result<Vec<results::DeploymentStatus>, ProtocolError> {
        let expired = self.deployments.expire_previews()?;
        for deployment in &expired {
            self.notify(deployment_event("preview.expired", deployment, None));
        }
        Ok(expired)
    }

    pub fn expire_previews_step(
        &self,
        cursor: Option<String>,
    ) -> Result<Option<String>, ProtocolError> {
        let (expired, next) = self.deployments.expire_previews_step(cursor)?;
        for deployment in &expired {
            self.notify(deployment_event("preview.expired", deployment, None));
        }
        Ok(next)
    }

    pub fn deliver_review_reminders(&self) -> Result<(), ProtocolError> {
        self.reviews.review_reminders(
            &self.events,
            (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
        )
    }

    pub fn refresh_agent_capabilities(&self) -> Result<(), ProtocolError> {
        let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
        self.agent_routing
            .refresh_capabilities(&self.config, now_ms)
    }

    fn capture_usage_context(&self, params: &Value, caller: &Caller) {
        if !caller.via_edge
            && let Some(work) = &caller.work
            && work.diagnostic.is_none()
            && work
                .context
                .as_ref()
                .is_some_and(|context| context.operation_id.is_some() && context.validate().is_ok())
            && let Ok(repository) = self
                .resolve_registered_repository(
                    params.get("path").and_then(Value::as_str),
                    params.get("repository_id").and_then(Value::as_str),
                    caller,
                )
                .or_else(|error| {
                    // Preserve declaration capture for a first registration. An
                    // existing worktree's optional accounting never updates it.
                    if error.code == ErrorCode::RepositoryNotFound {
                        self.resolve_repository(
                            params.get("path").and_then(Value::as_str),
                            params.get("repository_id").and_then(Value::as_str),
                            caller,
                            false,
                        )
                    } else {
                        Err(error)
                    }
                })
        {
            let now = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
            if crate::usage_context::ingest(
                &self.database,
                caller.uid,
                &repository.repository_id,
                work,
                now,
            )
            .is_err()
            {
                self.usage
                    .binding_failed(caller.uid, repository.repository_id);
            }
        }
    }

    fn dispatch_authorized(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let id = OperationId::from_name(operation)
            .ok_or_else(|| ProtocolError::new(ErrorCode::OperationUnknown, "unknown operation"))?;
        let now = self.timestamp()?;
        let actor = caller.actor();
        let administrator = if operation.starts_with("ticket.") {
            let principal = self.access.principal(caller)?;
            principal.local || principal.administrator
        } else {
            false
        };
        let glossary_repository = if operation.starts_with("glossary.") {
            let path = params.get("path").and_then(Value::as_str);
            let repository_id = params.get("repository_id").and_then(Value::as_str);
            if path.is_some() && repository_id.is_some() {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "Choose a project path or identity, not both",
                ));
            }
            let repository = if path.is_some() || repository_id.is_some() {
                Some(self.resolve_repository(path, repository_id, caller, true)?)
            } else {
                None
            };
            repository.map(|value| value.repository_id)
        } else {
            None
        };
        devcoordinator2_api::dispatch_operation(
            self,
            id,
            params,
            &HandlerContext {
                caller,
                now,
                actor,
                administrator,
                repository: glossary_repository,
            },
        )
    }

    fn resolve_repository(
        &self,
        path: Option<&str>,
        repository_id: Option<&str>,
        caller: &Caller,
        allow_archived: bool,
    ) -> Result<RepositoryIdentity, ProtocolError> {
        if let Some(repository_id) = repository_id {
            return self.repository_by_id(repository_id, allow_archived);
        }
        let path = path.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path or repository_id is required",
            )
        })?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be absolute",
            ));
        }
        if allow_archived {
            match self
                .registry
                .repository_status(&path, Some((caller.uid, caller.gid)))
            {
                Ok(status) => {
                    return Ok(RepositoryIdentity {
                        repository_id: status.repository_id,
                        display_name: status.display_name,
                    });
                }
                Err(error) if error.code == ErrorCode::RepositoryNotFound => {}
                Err(error) => return Err(error),
            }
        }
        let registered = self.registry.register(&path, caller.uid, caller.gid)?;
        Ok(RepositoryIdentity {
            repository_id: registered.repository_id,
            display_name: registered.display_name,
        })
    }

    fn resolve_registered_repository(
        &self,
        path: Option<&str>,
        repository_id: Option<&str>,
        caller: &Caller,
    ) -> Result<RepositoryIdentity, ProtocolError> {
        if let Some(repository_id) = repository_id {
            return self.repository_by_id(repository_id, false);
        }
        let path = path
            .filter(|path| Path::new(path).is_absolute())
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "an absolute project path or identity is required",
                )
            })?;
        let path = std::fs::canonicalize(path).map_err(|_| {
            ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                "no registered worktree matches this request",
            )
        })?;
        let registered = if let Some(registered) = self.registry.registered_worktree(&path)? {
            registered
        } else {
            // Subdirectories and file paths retain Git's canonical repository
            // semantics; the normal exact-root read requires no subprocess.
            let source =
                crate::repository::resolve_worktree(&path, Some((caller.uid, caller.gid)))?;
            self.registry
                .registered_worktree(&source.worktree_root)?
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryNotFound,
                        "no registered worktree matches this request",
                    )
                })?
        };
        Ok(RepositoryIdentity {
            repository_id: registered.repository_id,
            display_name: registered.display_name,
        })
    }

    fn repository_by_id(
        &self,
        repository_id: &str,
        allow_archived: bool,
    ) -> Result<RepositoryIdentity, ProtocolError> {
        let repository_id = repository_id.to_owned();
        let lookup = repository_id.clone();
        let state = self
            .database
            .read(move |connection| {
                connection
                    .query_row(
                        "SELECT archived_at,merged_into_repository_id,display_name FROM repositories WHERE repository_id=$1",
                        [&lookup],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    format!("no repository {repository_id}"),
                )
            })?;
        if !allow_archived && state.0.is_some() {
            let suffix = state
                .1
                .map(|replacement| format!("; use {replacement}"))
                .unwrap_or_default();
            return Err(ProtocolError::new(
                ErrorCode::RepositoryArchived,
                format!("repository {repository_id} is archived{suffix}"),
            ));
        }
        Ok(RepositoryIdentity {
            repository_id,
            display_name: state.2,
        })
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "cannot format operation timestamp",
                )
                .with_detail(error.to_string())
            })
    }

    fn authorize_telegram_chat(&self, caller: &Caller, chat_id: i64) -> Result<(), ProtocolError> {
        let principal = self.access.principal(caller)?;
        if principal.local || principal.administrator {
            return Ok(());
        }
        if self.telegram.chat_email(chat_id)? == principal.identity {
            Ok(())
        } else {
            Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "chat is linked to another identity",
            ))
        }
    }

    fn release_event_identity(&self, release_id: &str) -> Result<(String, String), ProtocolError> {
        let release_id = release_id.to_owned();
        let lookup = release_id.clone();
        self.database
            .read(move |connection| {
                connection
                    .query_row(
                        "SELECT repository_id,name FROM releases WHERE release_id=$1",
                        [&lookup],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ReleaseNotFound,
                    format!("no release {release_id}"),
                )
            })
    }

    fn authorize_telegram_scope(&self, caller: &Caller, scope: &str) -> Result<(), ProtocolError> {
        let principal = self.access.principal(caller)?;
        if principal.local || principal.administrator {
            return Ok(());
        }
        match parse_scope(scope)? {
            TelegramScope::Server => Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "server scope requires administrator",
            )),
            TelegramScope::Deployment(deployment_id) => {
                if principal.at_least(&deployment_id, &params::AccessRole::Viewer) {
                    Ok(())
                } else {
                    Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "viewer on the deployment required",
                    ))
                }
            }
            TelegramScope::Repository(repository_id) => {
                let deployments = principal.grants.keys().cloned().collect::<Vec<_>>();
                let visible = self.database.read(move |connection| {
                    for deployment in deployments {
                        let found = connection.query_row(
                            "SELECT EXISTS(SELECT 1 FROM deployments WHERE deployment_id=$1 AND repository_id=$2) OR EXISTS(SELECT 1 FROM observed_deployments WHERE observed_deployment_id=$1 AND repository_id=$2)",
                            crate::database::params![deployment, repository_id],
                            |row| row.get::<_, i64>(0),
                        )? != 0;
                        if found {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }).map_err(database_error)?;
                if visible {
                    Ok(())
                } else {
                    Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "no viewable deployment in that repository",
                    ))
                }
            }
        }
    }

    fn notify(&self, event: TelegramEvent) {
        if let Some(owned) = owned_notification(&event) {
            self.publish_owned(owned, None);
        }
        let _ = self.telegram.enqueue_event(&event);
    }

    fn publish_owned(&self, event: results::OwnedEvent, dedupe_key: Option<String>) {
        if let Ok(occurred_at) = self.timestamp() {
            let _ = self.events.publish(NewEvent {
                occurred_at,
                event,
                dedupe_key,
            });
        }
    }

    fn publish_planning(
        &self,
        kind: &str,
        repository_id: &str,
        subject_kind: &str,
        subject_id: &str,
        status: Option<String>,
    ) {
        self.publish_owned(
            results::OwnedEvent::Planning(results::PlanningOwnedEvent {
                kind: kind.to_owned(),
                repository_id: repository_id.to_owned(),
                subject_kind: subject_kind.to_owned(),
                subject_id: subject_id.to_owned(),
                status,
            }),
            None,
        );
    }

    fn publish_feedback(
        &self,
        kind: &str,
        repository_id: &str,
        feedback: &results::Feedback,
        comment_id: Option<String>,
    ) {
        self.publish_owned(
            results::OwnedEvent::Feedback(results::FeedbackOwnedEvent {
                kind: kind.to_owned(),
                repository_id: repository_id.to_owned(),
                feedback_id: feedback.feedback_id.clone(),
                task_id: feedback.task_id.clone(),
                comment_id,
                state: Some(feedback.state.clone()),
            }),
            None,
        );
    }
}

struct HandlerContext<'a> {
    caller: &'a Caller,
    now: String,
    actor: String,
    administrator: bool,
    repository: Option<String>,
}
macro_rules! handlers {
    ($this:ident,$params:ident,$caller:ident,$now:ident,$actor:ident,$administrator:ident,$repository:ident,$operation:ident;
     $($id:ident => $body:expr;)*) => {
        impl<'a> devcoordinator2_api::OperationHandler<HandlerContext<'a>> for ControlPlane {
            $(#[allow(non_snake_case,unused_variables)]
            fn $id(&$this,$params:<devcoordinator2_api::operation_types::$id as devcoordinator2_api::OperationContract>::Input,
                context:&HandlerContext<'a>)->Result<<devcoordinator2_api::operation_types::$id as devcoordinator2_api::OperationContract>::Output,ProtocolError> {
                let $caller=context.caller;let $now=context.now.clone();let $actor=context.actor.clone();
                let $administrator=context.administrator;let $repository=context.repository.as_deref();
                let $operation=<devcoordinator2_api::operation_types::$id as devcoordinator2_api::OperationContract>::NAME;
                $body
            })*
        }
    };
}
handlers! { self,params,caller,now,actor,administrator,repository,operation;
    StorageInventory => self.storage.inventory(params);
    StorageArtifactGet => {
                let p: devcoordinator2_api::storage::Reference = params;
                self.storage.artifact(&p.artifact_id)
            };
    StorageScan => self.storage.scan(params, caller);
    StorageRegister => self.storage.register(params, caller);
    StorageLegacyRegister => {
                self.storage.register_legacy(params, caller)
            };
    StorageProtectionSet => {
                self.storage.protect(params, caller)
            };
    StoragePolicyGet => self.storage.policy(params);
    StoragePolicySet => {
                self.storage.set_policy(params, caller)
            };
    StorageRootsList => self.storage.roots();
    StorageRootsSet => self.storage.set_root(params, caller);
    StorageCleanupPlan => self.storage.plan(params, caller);
    StorageCleanupStart => {
                self.storage.start(params, caller)
            };
    StorageJobStatus => {
                let p: devcoordinator2_api::storage::JobReference = params;
                self.storage.job(&p.job_id)
            };
    StorageJobCancel => self.storage.cancel(params, caller);
    StorageHistory => self.storage.history(params);
    StorageLeaseSet => self.storage.lease(params, caller);
    StorageLeaseRelease => {
                self.storage.release_lease(params, caller)
            };
    TicketRequest => {
                self.ticket_service()?.request(params, administrator)
            };
    TicketSettings => self.ticket_service()?.settings(administrator);
    TicketConfigure => self.ticket_service()?.configure(params);
    TicketRemote => self.ticket_service()?.remote(params);
    GlossaryList => {
                self.glossary.list(repository, params)
            };
    GlossaryResolve => {
                self.glossary.list(repository, params)
            };
    GlossaryGet => self.glossary.get(repository, params);
    GlossarySave => {
                self.glossary
                        .save(repository, params, &actor, &now)
            };
    GlossaryConfigure => {
                self.glossary
                        .configure(repository, params, &actor, &now)
            };
    GlossaryInherit => {
                self.glossary
                        .inherit(repository, params, &actor, &now)
            };
    GlossaryHistory => {
                self.glossary.history(repository, params)
            };
    GlossaryCheck => self.glossary.check(repository, params);
    GlossaryImpact => self.glossary.impact(params);
    AgentHubRegister => {
                self.agent_hub.register(params, caller)
            };
    AgentHubHeartbeat => {
                self.agent_hub.heartbeat(params, caller)
            };
    AgentHubUnregister => {
                self.agent_hub.unregister(params, caller)
            };
    AgentHubDirectory => self.agent_hub.directory(params);
    AgentHubRoomCreate => {
                self.agent_hub.room_create(params, caller)
            };
    AgentHubRoomList => {
                self.agent_hub.room_list(params, caller)
            };
    AgentHubRoomMemberAdd => {
                self.agent_hub.room_member_add(params, caller)
            };
    AgentHubRoomMemberRemove => {
                self.agent_hub.room_member_remove(params, caller)
            };
    AgentHubSend => self.agent_hub.send(params, caller);
    AgentHubReceive => {
                self.agent_hub.receive(params, caller)
            };
    AgentHubAck => self.agent_hub.ack(params, caller);
    AgentHubUploadStart => {
                self.agent_hub.upload_start(params, caller)
            };
    AgentHubUploadChunk => {
                self.agent_hub.upload_chunk(params, caller)
            };
    AgentHubUploadFile => {
                self.agent_hub.upload_file(params, caller)
            };
    AgentHubUploadRemove => {
                self.agent_hub.upload_remove(params, caller)
            };
    AgentHubConsoleDirectory => {
                self.agent_hub.console_directory(params)
            };
    AgentHubConsoleReceive => {
                self.agent_hub.console_receive(params)
            };
    AgentHubConsoleSend => {
                self.agent_hub.console_send(params)
            };
    AgentHubAdminAgentRemove => {
                self.agent_hub.admin_remove_agent(params)
            };
    AgentHubAdminRoomRemove => {
                self.agent_hub.admin_remove_room(params)
            };
    AgentHubAdminMessageRemove => {
                self.agent_hub.admin_remove_message(params)
            };
    AgentHubSettingsGet => {

                self.agent_hub.settings()
            };
    AgentHubSettingsSet => {
                self.agent_hub.settings_set(params)
            };
    AgentHubKeyRotate => self.agent_hub.key_rotate(params);
    AgentHubActivity => self.agent_hub.activity(params);
    AgentHubRemote => self.agent_hub.remote(params, caller);
    Ping => {

                Ok(PingData {
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: devcoordinator2_api::PROTOCOL_VERSION,
                    schema_version: DATABASE_SCHEMA_VERSION,
                    executor_schema: devcoordinator2_executor_protocol::EXECUTOR_SCHEMA,
                    source_commit: SOURCE_COMMIT.to_owned(),
                    socket: self.config.socket_path.display().to_string(),
                })
            };
    AgentSettingsGet => {
                let p: devcoordinator2_api::params::AgentSettingsGet = params;
                if p.scope == devcoordinator2_api::agent_routing::Scope::Repository {
                    self.repository_by_id(
                        p.repository_id.as_deref().ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::ParamsInvalid,
                                "repository settings require a repository",
                            )
                        })?,
                        false,
                    )?;
                }
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                self.agent_routing
                        .settings(p.scope, p.repository_id, p.harness, now_ms)
            };
    AgentSettingsSave => {
                let p: devcoordinator2_api::params::AgentSettingsSave = params;
                if p.scope == devcoordinator2_api::agent_routing::Scope::Repository {
                    self.repository_by_id(
                        p.repository_id.as_deref().ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::ParamsInvalid,
                                "repository settings require a repository",
                            )
                        })?,
                        false,
                    )?;
                }
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                self.agent_routing.save_settings(p, &actor, now_ms)
            };
    AgentRolesSave => {
                let p: devcoordinator2_api::params::AgentRolesSave = params;
                let (revision, roles) = self.agent_routing.save_roles(p, &actor)?;
                Ok(results::AgentRolesSaved { revision, roles })
            };
    AgentCapabilitiesReport => {
                let p: devcoordinator2_api::params::AgentCapabilitiesReport = params;
                if !caller.is_local() {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "capability reports are accepted from local harnesses only",
                    ));
                }
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                self.agent_routing.report_capabilities(
                    p,
                    caller.client_kind,
                    &actor,
                    now_ms,
                )
            };
    AgentInstructionCurrent => {
                let p: devcoordinator2_api::params::AgentInstructionCurrent = params;
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                self.agent_routing.instruction(
                    p.repository_id,
                    caller.client_kind,
                    caller.model.clone(),
                    caller.effort.clone(),
                    now_ms,
                )
            };
    ConfigGet => {

                self.deployments.configuration().get()
            };
    ConfigReload => {
                let actor = caller
                    .identity
                    .clone()
                    .unwrap_or_else(|| format!("uid:{}", caller.uid));
                self.deployments
                        .configuration()
                        .reload(&params.expected_revision, &actor)
            };
    ConfigEnvSet => self.deployments
                    .set_compose_authorization(params, caller);
    DeploymentPreflight => {
                self.deployments.preflight(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.candidate.as_ref(),
                    caller,
                )
            };
    UserWhoami => {

                self.access.who_am_i(caller)
            };
    UserAcceptInvitation => {
                self.access.accept_invitation(params)
            };
    UserList => {

                self.access.list_users()
            };
    UserInvite => {
                let result = self.access.invite(params, caller)?;
                self.notify(TelegramEvent::new("user.invited").with("email", result.email.clone()));
                Ok(result)
            };
    UserRemove => {
                let result = self.access.remove_user(params, caller)?;
                self.notify(TelegramEvent::new("user.removed").with("email", result.email.clone()));
                Ok(result)
            };
    GrantSet => {
                let result = self.access.set_grant(params, caller)?;
                self.notify(
                    TelegramEvent::new("grant.set")
                        .with("email", result.email.clone())
                        .with("deployment_id", result.deployment_id.clone()),
                );
                Ok(result)
            };
    GrantRemove => {
                let result = self.access.remove_grant(params, caller)?;
                self.notify(
                    TelegramEvent::new("grant.removed")
                        .with("email", result.email.clone())
                        .with("deployment_id", result.deployment_id.clone()),
                );
                Ok(result)
            };
    RepositoryRegister => {
                let result =
                    self.registry
                        .register(Path::new(&params.path), caller.uid, caller.gid)?;
                self.health.sampler().request_storage();
                if result.registered {
                    self.publish_owned(
                        results::OwnedEvent::Other(results::OtherOwnedEvent {
                            review: None,
                            kind: "repository.registered".to_owned(),
                            repository_id: Some(result.repository_id.clone()),
                            deployment_id: None,
                            subject_kind: "repository".to_owned(),
                            subject_id: result.repository_id.clone(),
                        }),
                        None,
                    );
                }
                Ok(result)
            };
    RepositoryList => {
                self.registry.list_repositories_page(params)
            };
    RepositoryStatus => {
                let mut result = self
                    .registry
                    .repository_status(Path::new(&params.path), Some((caller.uid, caller.gid)))?;
                result.current_test = self.tests.current_summary_ref(&params.path, caller);
                Ok(result)
            };
    RepositoryPresentationUpdate => {
                self.repository_by_id(&params.repository_id, false)?;
                let result = self.registry.update_presentation(params, caller.uid)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "repository.presentation.updated".to_owned(),
                        repository_id: Some(result.repository_id.clone()),
                        deployment_id: None,
                        subject_kind: "repository".to_owned(),
                        subject_id: result.repository_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    RepositoryArchive => {
                let result = self.registry.archive(
                    &params.repository_id,
                    &params.merged_into_repository_id,
                    &params.note,
                    caller.uid,
                )?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "repository.archived".to_owned(),
                        repository_id: Some(result.repository_id.clone()),
                        deployment_id: None,
                        subject_kind: "repository".to_owned(),
                        subject_id: result.repository_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    RepositoryUnarchive => {
                let result =
                    self.registry
                        .unarchive(&params.repository_id, &params.note, caller.uid)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "repository.unarchived".to_owned(),
                        repository_id: Some(result.repository_id.clone()),
                        deployment_id: None,
                        subject_kind: "repository".to_owned(),
                        subject_id: result.repository_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    RepositoryRetire => {
                let result = self.registry.retire(
                    &params.repository_id,
                    &params.note,
                    &params.idempotency_key,
                    caller.uid,
                    caller.gid,
                )?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "repository.retired".to_owned(),
                        repository_id: Some(result.repository_id.clone()),
                        deployment_id: None,
                        subject_kind: "repository".to_owned(),
                        subject_id: result.repository_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    ServerRegister => self.servers.register(params);
    ServerList => self.servers.list(params);
    ServerStop => self.servers.stop(params);
    DeploymentList => self.deployments.list(params, caller);
    DeploymentStatus => {
                self.deployments.status(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    caller,
                )
            };
    TestAdmissionStatus => {
                let _: params::TestAdmissionStatus = params;
                self.tests.admission_status()
            };
    DeploymentApply => {
                let result = self.deployments.apply_request(&params, caller)?;
                self.health.sampler().request_storage();
                self.notify(deployment_event("deployment.applied", &result, None));
                Ok(result)
            };
    DeploymentRollback => {
                let result = self.deployments.rollback(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    caller,
                )?;
                self.health.sampler().request_storage();
                self.notify(deployment_event("deployment.rolled_back", &result, None));
                Ok(result)
            };
    DeploymentLogs => {
                self.deployments.logs(params, caller)
            };
    DeploymentStart => {
                let action = operation.split('.').nth(1).unwrap_or_default();
                let result = self.deployments.control(
                    action,
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.component.as_deref(),
                    caller,
                )?;
                self.notify(deployment_event(operation, &result, params.component));
                Ok(result)
            };
    DeploymentStop => {
                let action = operation.split('.').nth(1).unwrap_or_default();
                let result = self.deployments.control(
                    action,
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.component.as_deref(),
                    caller,
                )?;
                self.notify(deployment_event(operation, &result, params.component));
                Ok(result)
            };
    DeploymentRestart => {
                let action = operation.split('.').nth(1).unwrap_or_default();
                let result = self.deployments.control(
                    action,
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.component.as_deref(),
                    caller,
                )?;
                self.notify(deployment_event(operation, &result, params.component));
                Ok(result)
            };
    DeploymentRemove => {
                let result = self.deployments.remove(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.delete_data,
                    caller,
                )?;
                self.health.sampler().request_storage();
                self.notify(
                    TelegramEvent::new("deployment.removed")
                        .with("deployment_id", result.deployment_id.clone())
                        .with("data_deleted", result.data_deleted),
                );
                Ok(result)
            };
    DeploymentSetDomain => {
                let deployment_id = params.deployment_id.clone();
                let result = self.deployments.set_domain(params)?;
                self.notify(
                    TelegramEvent::new("deployment.domain_changed")
                        .with("deployment_id", deployment_id)
                        .with("domain", result.domain.clone()),
                );
                Ok(result)
            };
    TestStart => {
                let result = self.tests.start(params, caller)?;
                Ok(result)
            };
    TestContainerCreate => {
                self.test_containers.create(params, caller)
            };
    TestContainerStart => {
                self.test_containers.start(params, caller)
            };
    TestContainerInspect => {
                self.test_containers.inspect(params, caller)
            };
    TestContainerDiagnostic => {
                self.test_containers.diagnostic(params, caller)
            };
    TestContainerRemove => {
                self.test_containers.remove(params, caller)
            };
    TestContainerRetry => {
                self.test_containers.retry(params, caller)
            };
    TestRetry => {
                let result = self.tests.retry(params, caller)?;
                Ok(result)
            };
    TestStatus => {
                self.tests.status(&params.path, caller)
            };
    TestStop => {
                self.tests.stop(&params.path, params.reason, caller)
            };
    TestHistory => self.tests.history(params, caller);
    TestList => {
                let mut result = self.tests.list_current_page(params)?;
                self.test_evidence.enrich_list(&mut result);
                Ok(result)
            };
    TestEvidenceGet => {
                self.test_evidence.get(params, caller)
            };
    TestEvidenceLookup => {
                self.test_evidence.lookup(params, caller)
            };
    TestEvidenceImage => {
                self.test_evidence.image(params, caller)
            };
    TestEvidenceFeedbackCreate => {
                let repository_id = self
                    .test_evidence
                    .repository_id_for(Path::new(&params.path), caller)?;
                let result = self.test_evidence.create_feedback(params, caller)?;
                self.publish_feedback(
                    "feedback.created",
                    &repository_id,
                    &result.feedback,
                    result
                        .feedback
                        .comments
                        .last()
                        .map(|comment| comment.comment_id.clone()),
                );
                self.publish_planning(
                    "task.created",
                    &repository_id,
                    "task",
                    &result.task_id,
                    enum_text(&result.feedback.task_status),
                );
                Ok(result)
            };
    TestEvidenceFeedbackReply => {
                let repository_id = self
                    .test_evidence
                    .repository_id_for(Path::new(&params.path), caller)?;
                let result = self.test_evidence.reply(params, caller)?;
                self.publish_feedback(
                    "feedback.replied",
                    &repository_id,
                    &result.feedback,
                    result
                        .feedback
                        .comments
                        .last()
                        .map(|comment| comment.comment_id.clone()),
                );
                Ok(result)
            };
    TestEvidenceFeedbackEdit => {
                let comment_id = params.comment_id.clone();
                let repository_id = self
                    .test_evidence
                    .repository_id_for(Path::new(&params.path), caller)?;
                let result = self.test_evidence.edit(params, caller)?;
                self.publish_feedback(
                    "feedback.edited",
                    &repository_id,
                    &result.feedback,
                    Some(comment_id),
                );
                self.publish_planning(
                    "task.updated",
                    &repository_id,
                    "task",
                    &result.feedback.task_id,
                    enum_text(&result.feedback.task_status),
                );
                Ok(result)
            };
    TestEvidenceFeedbackState => {
                let repository_id = self
                    .test_evidence
                    .repository_id_for(Path::new(&params.path), caller)?;
                let result = self.test_evidence.set_state(params, caller)?;
                self.publish_feedback(
                    "feedback.state_changed",
                    &repository_id,
                    &result.feedback,
                    None,
                );
                self.publish_planning(
                    "task.updated",
                    &repository_id,
                    "task",
                    &result.feedback.task_id,
                    enum_text(&result.feedback.task_status),
                );
                Ok(result)
            };
    TestEvidenceFeedbackDelete => {
                let repository_id = self
                    .test_evidence
                    .repository_id_for(Path::new(&params.path), caller)?;
                let result = self.test_evidence.delete(params, caller)?;
                self.publish_feedback("feedback.deleted", &repository_id, &result.feedback, None);
                self.publish_planning(
                    "task.updated",
                    &repository_id,
                    "task",
                    &result.feedback.task_id,
                    enum_text(&result.feedback.task_status),
                );
                Ok(result)
            };
    DesignSketchPublish => {
                let repository_id = params.repository_id.clone();
                let result = self.sketches.publish(params, caller)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "sketch.published".to_owned(),
                        repository_id: Some(repository_id),
                        deployment_id: None,
                        subject_kind: "sketch_batch".to_owned(),
                        subject_id: result.batch_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    DesignSketchList => self.sketches.list(params);
    DesignSketchSearch => self.sketches.search(params);
    DesignSketchStory => self.sketches.story(params);
    DesignSketchResolve => self.sketches.resolve(params);
    DesignSketchGet => {
                self.sketches.get(params, &caller.actor())
            };
    DesignSketchImage => self.sketches.image(params);
    DesignSketchRecord => self.sketches.record(params);
    DesignSketchDecision => {
                let repository_id = params.repository_id.clone();
                let result = self.sketches.decision(params, caller)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "sketch.decision.changed".to_owned(),
                        repository_id: Some(repository_id),
                        deployment_id: None,
                        subject_kind: "sketch".to_owned(),
                        subject_id: result.sketch.sketch_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    DesignSketchActivate => {
                let repository_id = params.repository_id.clone();
                let result = self.sketches.activate(params, caller)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "sketch.activation.changed".to_owned(),
                        repository_id: Some(repository_id),
                        deployment_id: None,
                        subject_kind: "sketch_surface".to_owned(),
                        subject_id: result.surface_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    DesignSketchDescription => {
                let repository_id = params.repository_id.clone();
                let result = self.sketches.description(params, caller)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "sketch.description.changed".to_owned(),
                        repository_id: Some(repository_id),
                        deployment_id: None,
                        subject_kind: "sketch".to_owned(),
                        subject_id: result.sketch.sketch_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    DesignSketchAnnotationCreate => {
                let repository_id = params.repository_id.clone();
                let result = self.sketches.annotation_create(params, caller)?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "sketch.annotation.created".to_owned(),
                        repository_id: Some(repository_id),
                        deployment_id: None,
                        subject_kind: "sketch_annotation".to_owned(),
                        subject_id: result.annotation.annotation_id.clone(),
                    }),
                    None,
                );
                Ok(result)
            };
    AgentMessagePoll => self.sketches.message_poll(params);
    AgentMessageClaim => {
                self.sketches.message_claim(params, caller)
            };
    AgentMessageAck => {
                self.sketches.message_ack(params, caller)
            };
    TestLogCatalog => {
                decode_result(self.logs.query(
                    LogQueryOperation::Catalog,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )?)
            };
    TestLogTail => {
                decode_result(self.logs.query(
                    LogQueryOperation::Tail,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )?)
            };
    TestLogSearch => {
                decode_result(self.logs.query(
                    LogQueryOperation::Search,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )?)
            };
    TestLogRange => {
                decode_result(self.logs.query(
                    LogQueryOperation::Range,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )?)
            };
    TestLogFailureContext => {
                decode_result(self.logs.query(
                    LogQueryOperation::FailureContext,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )?)
            };
    TestLogRetentionGet => {

                self.logs.retention()
            };
    TestLogRetentionSet => {
                self.logs.set_retention(
                    params.max_age_seconds,
                    params.case_depth,
                    &actor,
                    &now,
                )
            };
    TestArtifactCatalog => {
                self.artifacts.catalog(params, caller)
            };
    TestArtifactFile => self.artifacts.file(params, caller);
    TestCapacityGet => {

                self.capacity.snapshot()
            };
    TestCapacitySet => {
                let cap = match params.cap {
                    params::CapacityCap::Value(cap) => Some(u32::from(cap)),
                    params::CapacityCap::Null => None,
                };
                self.capacity.set_cap(cap, &actor)
            };
    HealthSummary => {

                self.health.summary()
            };
    HealthIncidents => self.incidents.list(params);
    HealthIncidentUpdate => {
                let incident = self.incidents.update(params, &caller.actor())?;
                self.publish_owned(
                    results::OwnedEvent::Other(results::OtherOwnedEvent {
                        review: None,
                        kind: "health.incident.changed".into(),
                        repository_id: incident.repository_id.clone(),
                        deployment_id: incident.deployment_id.clone(),
                        subject_kind: "health_incident".into(),
                        subject_id: incident.incident_id.clone(),
                    }),
                    None,
                );
                Ok(incident)
            };
    HealthRepositories => {

                self.health.repositories()
            };
    HealthRepository => {
                self.health.repository(&params.path, caller)
            };
    HealthHistory => self.health.history(params);
    HealthContainers => {

                self.health.containers()
            };
    HealthContainerRemove => {
                self.health.remove_container(&params.container_id)
            };
    UsageRepositories => self.usage.repositories(params);
    UsageRepository => self.usage.repository(params);
    UsageRateCardList => self.rate_cards.list(params);
    UsageRateCardSet => {
                self.rate_cards.set(params, &actor, &now)
            };
    ProgressRepositories => {

                self.progress.repositories()
            };
    ProgressRepository => self.progress.repository(params);
    TelegramLink => {
                let principal = self.access.principal(caller)?;
                let email = params.email.or(principal.identity.clone()).ok_or_else(|| {
                    ProtocolError::new(ErrorCode::ParamsInvalid, "email is required")
                })?;
                let email = email.to_lowercase();
                if !principal.local
                    && !principal.administrator
                    && principal.identity.as_deref() != Some(&email)
                {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "may only link chats to yourself",
                    ));
                }
                self.telegram.link(&params.code, &email)
            };
    TelegramSubscribe => {
                self.authorize_telegram_chat(caller, params.chat_id)?;
                self.authorize_telegram_scope(caller, &params.scope)?;
                self.telegram.subscribe(params)
            };
    TelegramUnsubscribe => {
                self.authorize_telegram_chat(caller, params.chat_id)?;
                self.telegram.unsubscribe(params)
            };
    TelegramList => {

                let principal = self.access.principal(caller)?;
                let email = (!principal.local && !principal.administrator)
                    .then_some(principal.identity)
                    .flatten();
                self.telegram.list(email.as_deref())
            };
    DeploymentRecovery => {
                let request: devcoordinator2_api::runtime_recovery::Request = params;
                self.resolve_repository(None, Some(&request.repository_id), caller, false)?;
                self.deployments
                        .recover_saved_preview(request, caller, &actor, &now)
            };
    PlanRecovery => {
                let request: devcoordinator2_api::recovery::Request = params;
                self.resolve_repository(None, Some(&request.repository_id), caller, false)?;
                let result =
                    crate::planning_recovery::recover(&self.database, request, &actor, &now)?;
                if result.status == "applied" {
                    self.publish_planning(
                        "plan.recovered",
                        &result.repository_id,
                        "repository",
                        &result.repository_id,
                        Some("recovered".to_owned()),
                    );
                }
                Ok(result)
            };
    PlanOverview => {
                if params.path.is_none() && params.repository_id.is_none() {
                    return self.plan.overview_page(None, params);
                }
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                self.plan
                        .overview_page(Some(&repository.repository_id), params)
            };
    CompletionCheck => {
                let repository = self.resolve_repository(Some(&params.path), None, caller, true)?;
                let result = crate::completion::CompletionService::new(self.database.clone())
                    .check_path(
                        &repository.repository_id,
                        Path::new(&params.path),
                        params.manifest,
                    )?;
                Ok(result)
            };
    TaskSearch => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                self.plan.search_tasks(&repository.repository_id, params)
            };
    TaskHistory => {
                self.plan.task_history(&params.task_id)
            };
    TaskCreate => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    false,
                )?;
                let result =
                    self.plan
                        .create_task(&repository.repository_id, params, &actor, &now)?;
                self.publish_planning(
                    "task.created",
                    &result.repository_id,
                    "task",
                    &result.task_id,
                    enum_text(&result.status),
                );
                Ok(result)
            };
    TaskUpdate => {
                let result = self.plan.update_task(params, &actor, &now)?;
                self.publish_planning(
                    "task.updated",
                    &result.repository_id,
                    "task",
                    &result.task_id,
                    enum_text(&result.status),
                );
                Ok(result)
            };
    ReleaseCreate => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    false,
                )?;
                let result =
                    self.plan
                        .create_release(&repository.repository_id, params, &actor, &now)?;
                self.publish_planning(
                    "release.created",
                    &repository.repository_id,
                    "release",
                    &result.release_id,
                    enum_text(&result.status),
                );
                Ok(result)
            };
    ReleaseUpdate => {
                let result = self.plan.update_release(params, &actor, &now)?;
                if let Some(repository_id) = &result.repository_id {
                    self.publish_planning(
                        "release.updated",
                        repository_id,
                        "release",
                        &result.release_id,
                        enum_text(&result.status),
                    );
                }
                Ok(result)
            };
    ReleaseRequest => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    false,
                )?;
                let result =
                    self.plan
                        .request_release(&repository.repository_id, params, &actor, &now)?;
                self.publish_planning(
                    "release.requested",
                    &repository.repository_id,
                    "release",
                    &result.release_id,
                    enum_text(&result.status),
                );
                self.notify(
                    TelegramEvent::new("release.requested")
                        .with("repository_id", result.repository_id.clone())
                        .with("repository_name", repository.display_name)
                        .with("name", result.name.clone())
                        .with("release_id", result.release_id.clone()),
                );
                Ok(result)
            };
    ReleaseDeliver => {
                let release_id = params.release_id.clone();
                let (repository_id, name) = self.release_event_identity(&release_id)?;
                let result = self.plan.deliver_release(params, &actor, &now)?;
                self.publish_planning(
                    "release.delivered",
                    &repository_id,
                    "release",
                    &release_id,
                    enum_text(&result.status),
                );
                let mut event = TelegramEvent::new("release.delivered")
                    .with("repository_id", repository_id)
                    .with("release_id", release_id)
                    .with("name", name)
                    .with("port", result.port)
                    .with("dirty", result.dirty);
                if let Some(url) = &result.url {
                    event = event.with("url", url.clone());
                }
                self.notify(event);
                Ok(result)
            };
    DecisionTail => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                self.plan.decision_tail(&repository.repository_id, params)
            };
    PerformanceOverview => self.reviews.performance_overview(
                    params,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                );
    PerformanceReviews => {
                self.reviews.performance_reviews(params)
            };
    PerformanceReview => self.reviews.performance_review(
                    params,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                );
    ReviewDeliveryPending => {
                self.deliver_review_reminders()?;
                self.reviews.delivery_pending(
                    params,
                    (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
                )
            };
    ReviewPolicySet => self.reviews.policy_set(
                params,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
            );
    ReviewPolicyStatus => self.reviews.policy_status(
                params,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
            );
    ReviewDeliveryRegister => self.reviews.delivery_register(
                params,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
                crate::review::RegistrationMode::Replace,
            );
    ReviewPrepare => self.reviews.prepare(
                    params,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                );
    ReviewRecord => self.reviews.record(
                    params,
                    &actor,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                );
    ReviewShow => self.reviews.show(params);
    ReviewReceipt => self.reviews.receipt(params);
    ReleaseEvidence => self.deliveries.receipt(params);
    ReleaseDeliverEvidence => self.deliveries.deliver(
                    params,
                    caller,
                    &actor,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                );
    ReleaseEvidenceShow => self.deliveries.show(params);
    DecisionSearch => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                self.plan
                        .search_decisions(&repository.repository_id, params)
            };
    DecisionRecord => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    false,
                )?;
                let result =
                    self.plan
                        .record_decision(&repository.repository_id, params, &actor, &now)?;
                self.publish_planning(
                    "decision.recorded",
                    &repository.repository_id,
                    "decision",
                    &result.decision_id,
                    None,
                );
                Ok(result)
            };
    DecisionSummarize => {
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    false,
                )?;
                let result = self.plan.summarize_decisions(
                    &repository.repository_id,
                    params,
                    &actor,
                    &now,
                )?;
                self.publish_planning(
                    "decision.summarized",
                    &repository.repository_id,
                    "decision_summary",
                    &result.covers_through_seq.to_string(),
                    None,
                );
                Ok(result)
            };
    BugReport => {
                let result =
                    bugs::report(&self.config.bugs_dir, &params, &actor).map_err(bug_error)?;
                if !result.duplicate {
                    let mut event = TelegramEvent::new("bug.opened")
                        .with("component", result.component.clone())
                        .with("summary", result.summary.clone());
                    if let Some(repository_id) = &result.correlations.repository_id {
                        event = event.with("repository_id", repository_id.clone());
                    }
                    if let Some(deployment_id) = &result.correlations.deployment_id {
                        event = event.with("deployment_id", deployment_id.clone());
                    }
                    self.notify(event);
                }
                Ok(result)
            };
    BugList => {

                Ok(results::BugList {
                    bugs: bugs::list_open(&self.config.bugs_dir).map_err(bug_error)?,
                    // The backing host path is deliberately private.
                    store: None,
                })
            };
    BugClose => {
                let result =
                    bugs::close(&self.config.bugs_dir, &params.bug_id).map_err(bug_error)?;
                self.notify(
                    TelegramEvent::new("bug.closed")
                        .with("component", result.component.clone())
                        .with("summary", result.summary.clone()),
                );
                Ok(result)
            };
    EventWait => Err(ProtocolError::new(
                ErrorCode::InternalError,
                "passive waits require deferred dispatch",
            ));
    TestWait => Err(ProtocolError::new(
                ErrorCode::InternalError,
                "passive waits require deferred dispatch",
            ));
    TestAdmissionWait => Err(ProtocolError::new(
                ErrorCode::InternalError,
                "passive waits require deferred dispatch",
            ));
    DeploymentWait => Err(ProtocolError::new(
                ErrorCode::InternalError,
                "passive waits require deferred dispatch",
            ));
}

impl OperationExecutor for ControlPlane {
    fn prepare_admission(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Option<crate::daemon::AdmissionPreparation> {
        let definition = devcoordinator2_api::operation(operation)?;
        // Admission follows the catalog's deployment scope and execution
        // profile. Saved-runtime recovery is server-scoped but has the same
        // exact deployment target and mutation exclusion.
        if !definition.long_running()
            || (definition.policy.scope != devcoordinator2_api::Scope::Deployment
                && definition.id != OperationId::DeploymentRecovery)
        {
            return None;
        }
        let plane = self.clone();
        let operation = operation.to_owned();
        let caller = caller.clone();
        Some(Box::new(move || {
            let authorization = plane.access.authorize(&operation, &params, &caller)?;
            plane.deployments.prepare_mutation_admission(
                authorization.params.get("path").and_then(Value::as_str),
                authorization.params.get("name").and_then(Value::as_str),
                authorization
                    .params
                    .get("deployment_id")
                    .and_then(Value::as_str),
                &caller,
            )
        }))
    }

    fn execute(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let authorization = self.access.authorize(operation, &params, caller)?;
        self.capture_usage_context(&authorization.params, caller);
        let result = self.dispatch_authorized(operation, authorization.params.clone(), caller)?;
        // Only an actual native capability selects the alarm route; client names do not.
        if !caller.via_edge
            && !operation.starts_with("review.delivery.")
            && !self
                .config
                .socket_path
                .with_file_name("daemon.pre-cutover.sock")
                .exists()
            && let Some(work) = caller.work.as_ref().and_then(|work| work.context.as_ref())
            && let Some(capability) = &work.alarm
            && let Ok(repository) = self.resolve_registered_repository(
                authorization.params.get("path").and_then(Value::as_str),
                authorization
                    .params
                    .get("repository_id")
                    .and_then(Value::as_str),
                caller,
            )
        {
            let now = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
            let scope = devcoordinator2_api::review_policy::Scope {
                repository_id: repository.repository_id.clone(),
                workstream_id: authorization
                    .params
                    .get("workstream_id")
                    .map(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| work.workstream_id.clone()),
            };
            if self
                .reviews
                .policy_status(scope, now)
                .ok()
                .flatten()
                .is_some()
                && let Err(error) = self.reviews.delivery_register(
                    devcoordinator2_api::review_policy::Register {
                        repository_id: repository.repository_id,
                        workstream_id: authorization
                            .params
                            .get("workstream_id")
                            .map(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_else(|| work.workstream_id.clone()),
                        owner_thread_id: work.thread_id.clone(),
                        mode: devcoordinator2_api::review_policy::DeliveryMode::CodexAlarm,
                        alarm_namespace: capability.alarm_namespace.clone(),
                        capability_revision: capability.capability_revision,
                        lease_expires_at: capability.lease_expires_at,
                    },
                    now,
                    crate::review::RegistrationMode::Refresh,
                )
            {
                tracing::warn!(code=%error.code,"native review delivery registration unavailable; message fallback remains available");
            }
        }
        let mut result = authorization.apply_result(result)?;
        if !caller.via_edge
            && !operation.starts_with("event.")
            && !operation.starts_with("agent.message.")
            && let Ok(repository) = self.resolve_registered_repository(
                params.get("path").and_then(Value::as_str),
                params.get("repository_id").and_then(Value::as_str),
                caller,
            )
            && let Ok(messages) = self.sketches.message_poll_kind(
                params::AgentMessagePoll {
                    repository_id: repository.repository_id,
                    after_id: None,
                    limit: 2,
                },
                Some("performance_review.reminder"),
            )
            && !messages.messages.is_empty()
            && let Some(object) = result.as_object_mut()
        {
            object.insert("_agent_messages".into(), encode(messages.messages)?);
        }
        Ok(result)
    }

    fn defer(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
        wait: WaitContext,
    ) -> Option<Result<crate::daemon::DeferredOperation, ProtocolError>> {
        if !devcoordinator2_api::is_deferred_wait(operation) {
            return None;
        }
        Some((|| {
            let authorization = self.access.authorize(operation, &params, caller)?;
            self.capture_usage_context(&authorization.params, caller);
            if operation == "test.admission.wait" {
                let request: params::TestAdmissionWait = decode(authorization.params)?;
                let deadline = parse_deadline(&request.deadline_at)?;
                let runtime = self.tests.admission_runtime_dir();
                let tests = self.tests.clone();
                return Ok(Box::pin(async move {
                    let admission_wait = tokio::time::timeout_at(
                        deadline,
                        crate::test_admission::wait_for_admission_open(runtime),
                    );
                    match admission_wait.await {
                        Ok(Ok(())) => {
                            wait.blocking(move || encode(tests.admission_status()?))
                                .await
                        }
                        Ok(Err(error)) => Err(ProtocolError::new(
                            ErrorCode::InternalError,
                            "test admission watcher failed",
                        )
                        .with_detail(error.to_string())),
                        Err(_) => Err(wait_deadline_error(
                            "test admission is still draining",
                            Some("draining".into()),
                            None,
                            None,
                        )),
                    }
                }) as crate::daemon::DeferredOperation);
            }
            if operation == "test.wait" {
                let request: params::TestWait = decode(authorization.params.clone())?;
                let current = self.tests.status(&request.path, caller)?;
                if current.run_id != request.run_id {
                    return Err(test_wait_run_changed(&request.run_id, &current));
                }
                if current.status != results::TestStatus::Running {
                    return Ok(Box::pin(async move {
                        encode(results::TestWaitResult {
                            completed: true,
                            timed_out: false,
                            status: current,
                        })
                    }) as crate::daemon::DeferredOperation);
                }
                let repository =
                    self.resolve_registered_repository(Some(&request.path), None, caller)?;
                let events = self.events.clone();
                let access = self.access.clone();
                let wait_caller = caller.clone();
                let tests = self.tests.clone();
                let run_id = request.run_id.clone();
                let path = request.path.clone();
                let deadline = request.deadline_at.clone();
                let deadline_at = parse_deadline(&deadline)?;
                let status_caller = caller.clone();
                return Ok(Box::pin(async move {
                    loop {
                        if tokio::time::Instant::now() >= deadline_at {
                            return Err(wait_deadline_error(
                                "test run did not finish before the requested deadline",
                                Some("running".into()),
                                Some(run_id.clone()),
                                None,
                            ));
                        }
                        let access_for_wait = access.clone();
                        let caller_for_wait = wait_caller.clone();
                        let subscription_events = events.clone();
                        let subscription_repository = repository.repository_id.clone();
                        let subscription_deadline =
                            crate::request_deadline::observation_deadline(&deadline)?;
                        let subscription = wait
                            .blocking(move || {
                                subscription_events.subscribe_with_visibility(
                                    params::EventWait {
                                        cursor: None,
                                        filters: vec![params::EventFilter {
                                            filter_id: "test-wait".into(),
                                            categories: vec![params::EventCategory::Test],
                                            kinds: vec!["test.finished".into()],
                                            repository_ids: vec![subscription_repository],
                                            deployment_ids: vec![],
                                            deadline_at: Some(subscription_deadline),
                                        }],
                                        limit: 10,
                                    },
                                    Arc::new(move || {
                                        access_for_wait.event_visibility(&caller_for_wait)
                                    }),
                                )
                            })
                            .await?;
                        // The run may finish between the initial status read and
                        // subscription creation. Re-check immediately after the
                        // subscription is installed so the completion event is
                        // not required to be observed by this connection.
                        let latest = test_wait_status(&tests, &path, &status_caller, &wait).await?;
                        if latest.run_id != run_id {
                            return Err(test_wait_run_changed(&run_id, &latest));
                        }
                        if latest.status != results::TestStatus::Running {
                            return encode(results::TestWaitResult {
                                completed: true,
                                timed_out: false,
                                status: latest,
                            });
                        }
                        let _ = subscription.receive().await?;
                    }
                }) as crate::daemon::DeferredOperation);
            }
            if operation == "deployment.wait" {
                let request: params::DeploymentWait = decode(authorization.params.clone())?;
                let access = self.access.clone();
                let wait_caller = caller.clone();
                return self.deployments.prepare_wait(
                    request,
                    caller.clone(),
                    self.events.clone(),
                    Arc::new(move || access.event_visibility(&wait_caller)),
                    wait,
                );
            }
            let request: params::EventWait = decode(authorization.params)?;
            let access = self.access.clone();
            let wait_caller = caller.clone();
            let subscription = self.events.subscribe_with_visibility(
                request,
                Arc::new(move || access.event_visibility(&wait_caller)),
            )?;
            Ok(
                Box::pin(async move { encode(subscription.receive().await?) })
                    as crate::daemon::DeferredOperation,
            )
        })())
    }
}

async fn test_wait_status(
    tests: &TestLifecycle,
    path: &str,
    caller: &Caller,
    wait: &WaitContext,
) -> Result<results::TestSummary, ProtocolError> {
    let tests = tests.clone();
    let path = path.to_owned();
    let caller = caller.clone();
    wait.blocking(move || tests.status(&path, &caller)).await
}

fn test_wait_run_changed(requested_run_id: &str, current: &results::TestSummary) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestNotFound,
        "the requested governed test run is no longer current",
    )
    .with_detail(format!(
        "requested run {requested_run_id}; current run {}",
        current.run_id
    ))
    .with_recovery(devcoordinator2_api::recovery::Guidance {
        class: devcoordinator2_api::recovery::Class::Conflict,
        retryable: false,
        waitable: false,
        safe_to_continue: true,
        state: Some(format!("{:?}", current.status)),
        reason: "The worktree has advanced to a different run; inspect history before choosing the next action.".into(),
        run_id: Some(requested_run_id.into()),
        deployment_id: None,
        generation: None,
        repository_id: None,
        options: vec![devcoordinator2_api::recovery::RecoveryOption {
            id: "inspect-history".into(),
            action: devcoordinator2_api::recovery::Action::Inspect,
            operation: Some("test.history".into()),
            effect: "Inspect the requested run in retained history and continue without starting a duplicate run.".into(),
            target: Some(requested_run_id.into()),
            cost: Some("immediate".into()),
            risk: Some("none".into()),
            prerequisites: vec!["use the same worktree path".into()],
            independent_work_safe: true,
            user_action_required: false,
        }],
        field_errors: vec![],
        example: None,
    })
}

struct RepositoryIdentity {
    repository_id: String,
    display_name: String,
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(params).map_err(|error| {
        ProtocolError::new(ErrorCode::ParamsInvalid, "operation parameters are invalid")
            .with_detail(error.to_string())
            .with_recovery(devcoordinator2_api::recovery::Guidance {
                class: devcoordinator2_api::recovery::Class::Invalid,
                retryable: false,
                waitable: false,
                safe_to_continue: true,
                state: Some("invalid_parameters".into()),
                reason: "Correct the named parameter fields and submit the operation again.".into(),
                run_id: None,
                deployment_id: None,
                generation: None,
                repository_id: None,
                options: vec![],
                field_errors: vec![devcoordinator2_api::recovery::FieldError {
                    field: "params".into(),
                    message: error.to_string(),
                }],
                example: Some(serde_json::json!({})),
            })
    })
}

fn event_timestamp(clock: &dyn Clock) -> String {
    clock
        .now_utc()
        .format(TIMESTAMP_FORMAT)
        .expect("static event timestamp format")
}

fn enum_text(value: &impl Serialize) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
}

fn decode_result<T: DeserializeOwned>(value: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value).map_err(|_| {
        ProtocolError::new(
            ErrorCode::InternalError,
            "domain result does not match the operation schema",
        )
    })
}

fn encode<T: Serialize>(result: T) -> Result<Value, ProtocolError> {
    serde_json::to_value(result).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot encode operation result")
            .with_detail(error.to_string())
    })
}

fn deployment_event(
    operation: &str,
    result: &results::DeploymentStatus,
    component: Option<String>,
) -> TelegramEvent {
    let source = match result.source {
        results::DeploymentSource::Worktree => "worktree",
        results::DeploymentSource::Checkout => "checkout",
        results::DeploymentSource::Observed => "observed",
    };
    TelegramEvent::new(operation)
        .with("deployment_id", result.deployment_id.clone())
        .with("name", result.name.clone())
        .with("source", source)
        .with("component", component)
        .with("repository_id", result.repository_id.clone())
        .with("state", result.state.clone())
        .with("generation", result.current_generation)
        .with("domain", result.domain.clone())
}

fn owned_alert_event(event: &AlertEvent) -> results::OwnedEvent {
    results::OwnedEvent::Health(results::HealthOwnedEvent {
        kind: event.kind.to_owned(),
        repository_id: (event.subject_kind == "repository").then(|| event.subject_id.clone()),
        deployment_id: (event.subject_kind == "deployment").then(|| event.subject_id.clone()),
        subject_kind: event.subject_kind.clone(),
        subject_id: event.subject_id.clone(),
        severity: event.severity.clone(),
    })
}

fn owned_notification(event: &TelegramEvent) -> Option<results::OwnedEvent> {
    let repository_id = event
        .fields
        .get("repository_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let deployment_id = event
        .fields
        .get("deployment_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if event.kind.starts_with("deployment.")
        || event.kind.starts_with("preview.")
        || event.kind.starts_with("component.")
    {
        return deployment_id.map(|deployment_id| {
            results::OwnedEvent::Deployment(results::DeploymentOwnedEvent {
                kind: event.kind.clone(),
                repository_id,
                deployment_id,
                component: event
                    .fields
                    .get("component")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                state: event
                    .fields
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        });
    }
    let (subject_kind, subject_id) = if event.kind.starts_with("user.") {
        ("access", "users".to_owned())
    } else if event.kind.starts_with("grant.") {
        (
            "access",
            deployment_id.clone().unwrap_or_else(|| "grants".to_owned()),
        )
    } else if event.kind.starts_with("bug.") {
        ("bug", "registry".to_owned())
    } else {
        return None;
    };
    Some(results::OwnedEvent::Other(results::OtherOwnedEvent {
        review: None,
        kind: event.kind.clone(),
        repository_id,
        deployment_id,
        subject_kind: subject_kind.to_owned(),
        subject_id,
    }))
}

fn bug_error(error: bugs::BugError) -> ProtocolError {
    ProtocolError::new(ErrorCode::ParamsInvalid, error.to_string())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "database operation failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventVisibility;
    use devcoordinator2_api::params::{
        AccessRole, InvitationGrant, InviteUser, TaskKind, TaskStatus,
    };
    use std::collections::HashSet;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn config(root: &Path) -> Config {
        Config {
            socket_path: root.join("daemon.sock"),
            sandbox_bridge_dir: std::path::PathBuf::from("/tmp/devcoordinator2-bridge"),
            state_dir: root.join("state"),
            unit_prefix: "devcoordinator2-test".into(),
            slice_name: "devcoordinator2-tests.slice".into(),
            client_group: "devcoordinator2-clients".into(),
            port_range: (40_000, 40_100),
            base_domain: "example.test".into(),
            edge_uid: Some(999),
            admin_emails: Vec::new(),
            telegram_token_file: None,
            telegram_api: "https://api.telegram.org".into(),
            bugs_dir: root.join("bugs"),
            compose_env_allowlist_file: None,
            compose_env_authorizations: HashSet::new(),
            codex_usage_sources_file: None,
            codex_usage_sources: Vec::new(),
        }
    }

    fn local() -> Caller {
        Caller {
            via_edge: false,
            pid: 1,
            uid: 1000,
            gid: 1000,
            client_kind: devcoordinator2_api::ClientKind::Codex,
            model: None,
            effort: None,
            client_session: Some("fixture".into()),
            work: None,
            identity: None,
        }
    }

    fn public(identity: &str) -> Caller {
        Caller {
            via_edge: false,
            pid: 2,
            uid: 999,
            gid: 999,
            client_kind: devcoordinator2_api::ClientKind::Edge,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: Some(identity.into()),
        }
    }

    #[test]
    fn composed_dispatch_authorizes_persists_and_publishes_typed_planning_results() {
        for base_domain in ["example.test", ""] {
            verify_composed_dispatch(base_domain);
        }
        verify_large_plan_pages();
        verify_deferred_wait_deadlines_and_cancellation();
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let mut configuration = config(temporary.path());
        configuration.base_domain = "https://example.test/invalid-path".into();
        let error = ControlPlane::new(configuration, database)
            .err()
            .expect("an invalid configured ticket origin must still be rejected");
        assert_eq!(error.code, ErrorCode::ParamsInvalid);
    }

    fn verify_deferred_wait_deadlines_and_cancellation() {
        use crate::deployment_state::{DeploymentStore, ObservedDeploymentInput};
        use crate::test_state::{TestRunStore, initial_summary};
        use std::task::Poll;
        use time::{OffsetDateTime, format_description::well_known::Rfc3339};

        let temporary = tempdir().unwrap();
        let worktree = temporary.path().join("repository");
        std::fs::create_dir(&worktree).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&worktree)
                .status()
                .unwrap()
                .success()
        );
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .unwrap();
        let caller = Caller {
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            ..local()
        };
        let repository = plane
            .registry
            .register(&worktree, caller.uid, caller.gid)
            .unwrap();
        let run_id = "t20260904T000000Z-aabbcc";
        let store = TestRunStore;
        let prepared = store
            .prepare(&worktree, run_id, caller.uid, caller.gid)
            .unwrap();
        let summary = initial_summary(
            run_id,
            "all",
            "2026-09-04T00:00:00Z",
            caller.uid,
            "codex",
            devcoordinator2_executor_protocol::ProofKind::Complete,
            vec![],
            None,
            devcoordinator2_executor_protocol::ValidationTier::Release,
        );
        store
            .write_summary(&prepared.current, &summary, caller.uid, caller.gid)
            .unwrap();
        let deployment_id = "d2222222222222222";
        DeploymentStore::new(database)
            .replace_observed_current(
                &[ObservedDeploymentInput {
                    deployment_id: deployment_id.into(),
                    repository_id: repository.repository_id,
                    name: "wait-fixture".into(),
                    native_project: "wait-fixture".into(),
                    state: "stopped".into(),
                    health: "unknown".into(),
                    evidence: serde_json::json!({"source": "isolated-wait-fixture"}),
                }],
                &[],
                &[],
                "2026-09-04T00:00:00Z",
            )
            .unwrap();
        let lease = crate::test_admission::begin_drain(
            plane.tests.admission_runtime_dir(),
            "isolated wait fixture",
        )
        .unwrap();
        let past = (OffsetDateTime::now_utc() - time::Duration::seconds(1))
            .format(&Rfc3339)
            .unwrap();
        let future = (OffsetDateTime::now_utc() + time::Duration::hours(1))
            .format(&Rfc3339)
            .unwrap();
        let parameters = |operation: &str, deadline: &str| match operation {
            "event.wait" => {
                serde_json::json!({"filters": [{"filter_id": "deadline", "categories": ["health"], "deadline_at": deadline}], "limit": 1})
            }
            "test.admission.wait" => serde_json::json!({"deadline_at": deadline}),
            "test.wait" => {
                serde_json::json!({"path": worktree, "run_id": run_id, "deadline_at": deadline})
            }
            "deployment.wait" => {
                serde_json::json!({"deployment_id": deployment_id, "state": "ready", "deadline_at": deadline})
            }
            _ => unreachable!(),
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let wait_context = || WaitContext::new(Arc::new(tokio::sync::Semaphore::new(2)), None);
            for operation in [
                "event.wait",
                "test.admission.wait",
                "test.wait",
                "deployment.wait",
            ] {
                let mut waiting = plane
                    .defer(
                        operation,
                        parameters(operation, &future),
                        &caller,
                        wait_context(),
                    )
                    .unwrap()
                    .unwrap();
                std::future::poll_fn(|context| {
                    assert!(
                        waiting.as_mut().poll(context).is_pending(),
                        "{operation} must remain pending while its condition is unmet"
                    );
                    Poll::Ready(())
                })
                .await;
                // Exercise cancellation of the actual admission watcher or event
                // subscription, not only the socket fixture's stand-in future.
                drop(waiting);
                let expired = plane
                    .defer(
                        operation,
                        parameters(operation, &past),
                        &caller,
                        wait_context(),
                    )
                    .unwrap()
                    .unwrap();
                let result = tokio::time::timeout(std::time::Duration::from_secs(2), expired)
                    .await
                    .expect("the operation honors its declared deadline");
                if operation == "event.wait" {
                    let result = result.unwrap();
                    assert_eq!(result["heartbeat_due"][0]["filter_id"], "deadline");
                    assert_eq!(result["events"], serde_json::json!([]));
                    assert!(
                        result.get("completed").is_none(),
                        "a heartbeat is not completion"
                    );
                } else {
                    let error = result.unwrap_err();
                    assert_eq!(error.code, ErrorCode::WaitDeadlineReached, "{operation}");
                    let recovery = error.recovery.unwrap();
                    assert!(recovery.waitable && recovery.safe_to_continue);
                    assert!(!recovery.retryable);
                }
            }
            assert!(plane.tests.admission_status().unwrap().lease_live);
            assert_eq!(
                plane
                    .tests
                    .status(worktree.to_str().unwrap(), &caller)
                    .unwrap()
                    .status,
                results::TestStatus::Running
            );
            assert_eq!(
                plane
                    .deployments
                    .status(None, None, Some(deployment_id), &caller)
                    .unwrap()
                    .state,
                "stopped"
            );
            crate::test_admission::end_drain(&lease).unwrap();
            let reopened = plane
                .defer(
                    "test.admission.wait",
                    parameters("test.admission.wait", &future),
                    &caller,
                    wait_context(),
                )
                .unwrap()
                .unwrap()
                .await
                .unwrap();
            assert_eq!(reopened["state"], "open");
        });
    }

    fn verify_large_plan_pages() {
        use devcoordinator2_api::params::{ReleaseKind, ReleaseStatus};
        use devcoordinator2_api::results::{
            DecisionState, ElaborationRequest, PlanDetail, PlanRelease, PlanTask, PreviewRequest,
        };
        use devcoordinator2_api::{ResponseEnvelope, encode_response};

        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let repository_id = "r1111111111111111";
        let mut expected = PlanDetail {
            repository_id: repository_id.into(),
            display_name: "Большой план".into(),
            repository_source: None,
            presentation: None,
            archived: false,
            merged_into_repository_id: None,
            releases: Vec::new(),
            tasks: Vec::new(),
            tasks_truncated: false,
            elaboration_requests: Vec::new(),
            preview_requested: Vec::new(),
            decisions: DecisionState {
                unsummarized_count: 0,
                summary_due: false,
            },
            revision: String::new(),
            next_sequence: None,
            next_release_sequence: None,
        };
        for seq in 1..=160 {
            let release_id = format!("v{seq:016x}");
            let name = format!("Выпуск {seq}");
            let note = Some("🧭".repeat(490));
            expected.releases.push(PlanRelease {
                release_id: release_id.clone(),
                name: name.clone(),
                kind: ReleaseKind::Preview,
                status: ReleaseStatus::Requested,
                seq,
                note: note.clone(),
                requested_at: Some("t".into()),
                delivered_at: None,
                url: None,
                port: None,
                tasks_total: if seq == 1 { 559 } else { 0 },
                tasks_done: 0,
                loc_total: if seq == 1 { 559 * 120 } else { 0 },
                loc_done: 0,
            });
            expected.preview_requested.push(PreviewRequest {
                release_id,
                name,
                requested_at: "t".into(),
                note,
            });
        }
        for seq in 1..=560 {
            let task_id = format!("p{seq:016x}");
            let title = format!("Задача {seq} {}", "界".repeat(90));
            expected.tasks.push(PlanTask {
                task_id: task_id.clone(),
                parent_task_id: (seq != 1).then(|| "p0000000000000001".into()),
                release_id: Some("v0000000000000001".into()),
                seq,
                position: seq,
                title: title.clone(),
                impact: Some("Влияние \"\\\n".repeat(10)),
                status: TaskStatus::Planned,
                kind: TaskKind::Goal,
                estimated_loc: Some(120),
                elaboration_needed: seq <= 120,
            });
            if seq <= 120 {
                expected.elaboration_requests.push(ElaborationRequest {
                    task_id,
                    title,
                    outcome: "Результат ".repeat(180),
                    status: TaskStatus::Planned,
                    kind: TaskKind::Goal,
                    requested_at: None,
                });
            }
        }
        let fixture = expected.clone();
        database.transaction(move |transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES($1,'/repo',$2,'t',1000,'t')", crate::database::params![fixture.repository_id, fixture.display_name])?;
            transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111',$1,'/repo','t','t')", [&fixture.repository_id])?;
            for release in fixture.releases {
                transaction.execute("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,note,requested_at,created_at,created_by,updated_at) VALUES($1,$2,$3,$4,'preview','requested',$5,'t','t','uid:1000','t')", crate::database::params![release.release_id, fixture.repository_id, release.seq, release.name, release.note])?;
            }
            for task in fixture.tasks {
                transaction.execute("INSERT INTO tasks(task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'goal','planned',120,$10,'t','uid:1000','t')", crate::database::params![task.task_id, fixture.repository_id, task.parent_task_id, task.release_id, task.seq, task.position, task.title, "Результат ".repeat(180), task.impact, task.elaboration_needed])?;
            }
            Ok(())
        }).expect("large multilingual persisted plan");
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .expect("control plane");
        expected.revision = plane
            .execute(
                "plan.overview",
                serde_json::json!({"repository_id": repository_id}),
                &local(),
            )
            .unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let oversized = ResponseEnvelope::success("unpaged", &expected).unwrap();
        assert!(
            serde_json::to_vec(&oversized).unwrap().len() > devcoordinator2_api::MAX_RESPONSE_BYTES
        );
        let original_failure: serde_json::Value =
            serde_json::from_slice(&encode_response(&oversized)).unwrap();
        assert_eq!(
            original_failure["error"]["message"],
            "response exceeded size cap"
        );

        let mut merged = expected.clone();
        merged.releases.clear();
        merged.tasks.clear();
        merged.elaboration_requests.clear();
        merged.preview_requested.clear();
        let mut after_sequence = 0;
        let mut after_release_sequence = 0;
        let mut pages = 0;
        let mut first_next = None;
        loop {
            let value = plane.execute("plan.overview", serde_json::json!({"repository_id": repository_id, "after_sequence": after_sequence, "after_release_sequence": after_release_sequence, "expected_revision": expected.revision}), &local()).expect("page dispatch");
            let encoded = encode_response(&ResponseEnvelope::success("plan-page", &value).unwrap());
            assert!(encoded.len() <= devcoordinator2_api::MAX_RESPONSE_BYTES);
            let envelope: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(
                envelope["ok"], true,
                "every actual wire response must succeed"
            );
            let page: PlanDetail = serde_json::from_value(value).unwrap();
            assert_eq!(page.revision, expected.revision);
            assert_eq!(page.repository_id, expected.repository_id);
            assert_eq!(page.display_name, expected.display_name);
            assert_eq!(page.archived, expected.archived);
            assert_eq!(
                page.merged_into_repository_id,
                expected.merged_into_repository_id
            );
            assert_eq!(page.tasks_truncated, page.next_sequence.is_some());
            assert_eq!(page.decisions, expected.decisions);
            let count = page.releases.len()
                + page.tasks.len()
                + page.elaboration_requests.len()
                + page.preview_requested.len();
            assert!(count > 0, "continuations must make progress");
            merged.releases.extend(page.releases);
            merged.tasks.extend(page.tasks);
            merged
                .elaboration_requests
                .extend(page.elaboration_requests);
            merged.preview_requested.extend(page.preview_requested);
            pages += 1;
            if pages == 1 {
                first_next = Some((
                    page.next_sequence.unwrap_or(u32::MAX),
                    page.next_release_sequence.unwrap_or(u32::MAX),
                ));
            }
            if page.next_sequence.is_none() && page.next_release_sequence.is_none() {
                break;
            }
            let next = page.next_sequence.unwrap_or(u32::MAX);
            let next_release = page.next_release_sequence.unwrap_or(u32::MAX);
            assert!(next >= after_sequence && next_release >= after_release_sequence);
            assert!(next > after_sequence || next_release > after_release_sequence);
            after_sequence = next;
            after_release_sequence = next_release;
            assert!(pages < 30, "finite traversal");
        }
        assert!(
            pages >= 5,
            "all four large collections span bounded responses"
        );
        assert_eq!(
            merged, expected,
            "paging preserves every projected field, parent and aggregate"
        );
        for params in [
            serde_json::json!({"repository_id": repository_id, "offset": 1}),
            serde_json::json!({"repository_id": repository_id, "limit": 0}),
            serde_json::json!({"repository_id": repository_id, "limit": 201}),
            serde_json::json!({"repository_id": repository_id, "headers_only": true}),
        ] {
            assert_eq!(
                plane
                    .execute("plan.overview", params, &local())
                    .unwrap_err()
                    .code,
                ErrorCode::ParamsInvalid
            );
        }
        plane
            .execute(
                "task.update",
                serde_json::json!({"task_id": "p0000000000000001", "title": "Изменённая задача"}),
                &local(),
            )
            .expect("real persisted mutation");
        for (after_sequence, after_release_sequence) in [(0, 0), first_next.unwrap()] {
            assert_eq!(plane.execute("plan.overview", serde_json::json!({"repository_id": repository_id, "after_sequence": after_sequence, "after_release_sequence": after_release_sequence, "expected_revision": expected.revision}), &local()).unwrap_err().code, ErrorCode::CursorStale);
        }
        let fresh = plane
            .execute(
                "plan.overview",
                serde_json::json!({"repository_id": repository_id}),
                &local(),
            )
            .expect("restart after stale cursor");
        assert_ne!(fresh["revision"], expected.revision);
        database
            .transaction(|transaction| {
                transaction.execute(
                    "UPDATE releases SET note=$1 WHERE release_id='v0000000000000001'",
                    ["x".repeat(192 * 1024)],
                )?;
                Ok(())
            })
            .unwrap();
        let no_progress = plane
            .execute(
                "plan.overview",
                serde_json::json!({"repository_id": repository_id}),
                &local(),
            )
            .unwrap_err();
        assert_eq!(no_progress.code, ErrorCode::InternalError);
        assert_eq!(
            no_progress.message,
            "one plan record exceeds the response budget"
        );
    }

    fn verify_composed_dispatch(base_domain: &str) {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1000,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/repo','t','t')", [])?;
                Ok(())
            })
            .expect("fixture");
        let mut configuration = config(temporary.path());
        configuration.base_domain = base_domain.into();
        let plane = ControlPlane::with_adapters(
            configuration,
            database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .expect("control plane");

        assert!(
            plane
                .execute("ping", serde_json::json!({}), &local())
                .is_ok()
        );
        if base_domain.is_empty() {
            for (operation, parameters) in [
                ("ticket.settings", serde_json::json!({})),
                (
                    "ticket.configure",
                    serde_json::json!({"upstream":"local","expected_revision":0}),
                ),
                (
                    "ticket.request",
                    serde_json::json!({"target":"local","action":{"kind":"list"}}),
                ),
                (
                    "ticket.remote",
                    serde_json::json!({"action":{"kind":"list"}}),
                ),
            ] {
                let error = plane.execute(operation, parameters, &local()).unwrap_err();
                assert_eq!(error.code, ErrorCode::ConfigurationInvalid, "{operation}");
                assert!(error.message.contains("DEVCOORDINATOR2_BASE_DOMAIN"));
            }
        } else {
            let settings = plane
                .execute("ticket.settings", serde_json::json!({}), &local())
                .expect("configured ticket service");
            assert_eq!(settings["local"], "https://example.test");
        }
        let forbidden = plane
            .execute(
                "ticket.configure",
                serde_json::json!({"upstream":"local","expected_revision":0}),
                &public("visitor@example.test"),
            )
            .expect_err("domain availability must not bypass authorization");
        assert_eq!(forbidden.code, ErrorCode::PermissionDenied);

        let created = plane
            .execute(
                "task.create",
                serde_json::json!({
                    "repository_id":"r1111111111111111",
                    "title":"Port one complete operation",
                    "kind":"goal",
                    "estimated_loc":120
                }),
                &local(),
            )
            .expect("task create");
        assert_eq!(created["status"], serde_json::json!(TaskStatus::Planned));
        assert_eq!(created["repository_id"], "r1111111111111111");
        let event = plane
            .events
            .subscribe(
                params::EventWait {
                    cursor: Some(0),
                    filters: vec![params::EventFilter {
                        filter_id: "planning".into(),
                        categories: vec![params::EventCategory::Planning],
                        kinds: vec!["task.created".into()],
                        repository_ids: vec!["r1111111111111111".into()],
                        deployment_ids: Vec::new(),
                        deadline_at: None,
                    }],
                    limit: 100,
                },
                EventVisibility::unrestricted(),
            )
            .expect("event subscription")
            .blocking_receive()
            .expect("planning event");
        assert_eq!(event.events.len(), 1);
        assert_eq!(event.events[0].filter_ids, ["planning"]);
        assert!(matches!(
            &event.events[0].event.event,
            results::OwnedEvent::Planning(event)
                if event.subject_id == created["task_id"].as_str().unwrap()
        ));
        let overview = plane
            .execute(
                "plan.overview",
                serde_json::json!({"repository_id":"r1111111111111111"}),
                &local(),
            )
            .expect("overview");
        assert_eq!(
            overview["tasks"][0]["kind"],
            serde_json::json!(TaskKind::Goal)
        );
        assert_eq!(overview["tasks"][0]["estimated_loc"], 120);
        assert!(
            overview["next_offset"].is_null(),
            "small plans need only one page"
        );
        assert_eq!(overview["revision"].as_str().unwrap().len(), 64);

        let invalid_start = plane
            .execute("test.start", serde_json::json!({"path":"/repo"}), &local())
            .expect_err("invalid repository is rejected by the installed lifecycle");
        assert_eq!(invalid_start.code, ErrorCode::RepositoryNotFound);
        assert!(overview.get("_agent_messages").is_none());
        let now = (datetime!(2026-09-03 12:00 UTC).unix_timestamp_nanos() / 1_000_000) as u64;
        let mut native = local();
        native.client_session = Some("00000000-0000-0000-0000-000000000001".into());
        native.work=Some(devcoordinator2_api::work_context::WorkAttribution {
            context:Some(devcoordinator2_api::work_context::WorkContext::parse(&serde_json::json!({"version":1,"native_project_id":"project-native","thread_id":native.client_session,"workstream_id":"native","alarm":{"alarm_namespace":"codex.review.v1","capability_revision":1,"lease_expires_at":now+1000}}).to_string()).unwrap()),
            source:devcoordinator2_api::work_context::WorkSource::Request,diagnostic:None,
        });
        plane.execute("review.policy.set",serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"native","review_interval_ms":100,"active":true,"window_start_ms":now-100}),&native).unwrap();
        let policy = plane
            .execute(
                "review.policy.status",
                serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"native"}),
                &native,
            )
            .unwrap();
        assert_eq!(policy["delivery_route"], "codex_alarm");
        let mut second = native.clone();
        let other_thread = "00000000-0000-0000-0000-000000000002";
        second.client_session = Some(other_thread.into());
        second
            .work
            .as_mut()
            .unwrap()
            .context
            .as_mut()
            .unwrap()
            .thread_id = other_thread.into();
        let refreshed = plane
            .execute(
                "review.policy.status",
                serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"native"}),
                &second,
            )
            .unwrap();
        assert_eq!(
            refreshed["owner_thread_id"],
            native.client_session.as_ref().unwrap().as_str()
        );

        plane.deliver_review_reminders().unwrap();
        let pending = plane
            .execute(
                "review.delivery.pending",
                serde_json::json!({"alarm_namespace":"codex.review.v1"}),
                &native,
            )
            .unwrap();
        assert_eq!(pending["reminders"].as_array().unwrap().len(), 1);
        assert_eq!(
            pending["reminders"][0]["owner_thread_id"],
            native.client_session.as_ref().unwrap().as_str()
        );
        // The same Codex client name without the capability uses normal messages.
        let fallback=plane.execute("review.policy.set",serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"official","review_interval_ms":100,"active":true,"window_start_ms":now-100}),&local()).unwrap();
        assert_eq!(fallback["due"], true);
        plane.deliver_review_reminders().unwrap();
        let fallback = plane
            .execute(
                "review.policy.status",
                serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"official"}),
                &local(),
            )
            .unwrap();
        let envelope = serde_json::to_value(
            devcoordinator2_api::ResponseEnvelope::success("fixture", fallback).unwrap(),
        )
        .unwrap();
        assert_eq!(
            envelope["agent_messages"][0]["kind"],
            "performance_review.reminder"
        );
        assert!(envelope["data"].get("_agent_messages").is_none());
        let inactive=plane.execute("review.policy.set",serde_json::json!({"repository_id":"r1111111111111111","workstream_id":"official","active":false,"clock_action":"follow_existing"}),&local()).unwrap();
        assert!(inactive.get("_agent_messages").is_none());
        assert_eq!(inactive["due"], false);
        assert_eq!(inactive["last_completed_receipt"], serde_json::Value::Null);
    }

    #[test]
    fn agent_hub_operations_cross_the_typed_dispatch_boundary() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database,
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .expect("control plane");

        let first = plane
            .execute(
                "agent.hub.register",
                serde_json::json!({
                    "space":"private",
                    "display_name":"first",
                    "repository_label":"repo-one",
                    "domain_label":"example.test",
                    "current_subject":"dispatch journey",
                    "capabilities":["chat"]
                }),
                &local(),
            )
            .expect("first registration");
        let second = plane
            .execute(
                "agent.hub.register",
                serde_json::json!({
                    "space":"private",
                    "display_name":"second",
                    "repository_label":"repo-two",
                    "domain_label":"example.test",
                    "current_subject":"dispatch journey",
                    "capabilities":["chat"]
                }),
                &local(),
            )
            .expect("second registration");
        let auth = |value: &Value| {
            serde_json::json!({
                "agent_id": value["agent"]["agent_id"],
                "token": value["token"]
            })
        };
        let room = plane
            .execute(
                "agent.hub.room.create",
                serde_json::json!({
                    "auth":auth(&first),
                    "space":"private",
                    "name":"dispatch-room",
                    "members":[second["agent"]["agent_id"]]
                }),
                &local(),
            )
            .expect("room creation");
        let message = plane
            .execute(
                "agent.hub.send",
                serde_json::json!({
                    "auth":auth(&first),
                    "room_id":room["room_id"],
                    "body":"hello through dispatch",
                    "attachments":[],
                    "client_message_id":"dispatch-1"
                }),
                &local(),
            )
            .expect("message send");
        assert_eq!(message["body"], "hello through dispatch");
        let received = plane
            .execute(
                "agent.hub.receive",
                serde_json::json!({
                    "auth":auth(&second),
                    "room_id":room["room_id"],
                    "limit":20
                }),
                &local(),
            )
            .expect("message receive");
        assert_eq!(received["deliveries"].as_array().unwrap().len(), 1);
        plane
            .execute(
                "agent.hub.ack",
                serde_json::json!({
                    "auth":auth(&second),
                    "delivery_ids":[received["deliveries"][0]["delivery_id"]]
                }),
                &local(),
            )
            .expect("message acknowledgement");
    }

    #[test]
    fn native_mutation_events_follow_commits_and_reject_rolled_back_changes() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        database.transaction(|transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1000,'t')", [])?;
            transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/repo','t','t')", [])?;
            Ok(())
        }).unwrap();
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .unwrap();
        let count = |kind: &str| {
            let kind = kind.to_owned();
            database
                .call(move |connection| {
                    connection
                        .query_row(
                            "SELECT COUNT(*) FROM owned_events WHERE kind=$1",
                            [kind],
                            |row| row.get::<_, i64>(0),
                        )
                        .map_err(DatabaseError::from)
                })
                .unwrap()
        };
        for (operation, parameters, table, kind) in [
            (
                "task.create",
                serde_json::json!({"repository_id":"r1111111111111111","title":"One committed task","kind":"improvement"}),
                "plan_events",
                "task.created",
            ),
            (
                "release.create",
                serde_json::json!({"repository_id":"r1111111111111111","name":"A verified preview","kind":"preview"}),
                "releases",
                "release.created",
            ),
            (
                "decision.record",
                serde_json::json!({"repository_id":"r1111111111111111","aspect":"testing","title":"Commit before notifying","body":"A failed database transaction must never announce a successful change."}),
                "decisions",
                "decision.recorded",
            ),
            (
                "user.invite",
                serde_json::json!({"email":"events@example.invalid","administrator":false}),
                "invitations",
                "user.invited",
            ),
        ] {
            let trigger = format!(
                "CREATE OR REPLACE FUNCTION reject_event_fixture_func() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated transaction failure'; END; $$; CREATE TRIGGER reject_event_fixture BEFORE INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION reject_event_fixture_func();"
            );
            database
                .call(move |connection| {
                    connection.execute_batch(&trigger)?;
                    Ok(())
                })
                .unwrap();
            assert!(
                plane
                    .execute(operation, parameters.clone(), &local())
                    .is_err(),
                "{operation} accepted the failed transaction"
            );
            assert_eq!(
                count(kind),
                0,
                "{operation} published before its transaction committed"
            );
            database
                .call(|connection| {
                    connection
                        .execute_batch(&format!("DROP TRIGGER reject_event_fixture ON {table}"))?;
                    Ok(())
                })
                .unwrap();
            plane.execute(operation, parameters, &local()).unwrap();
            assert_eq!(
                count(kind),
                1,
                "{operation} did not publish exactly one committed event"
            );
        }
        let alerts = plane.health.sampler().alerts();
        let mut condition = crate::alerts::Condition {
            key: "host/event-test".into(),
            kind: "host_disk".into(),
            subject_kind: "host".into(),
            subject_id: "host".into(),
            severity: "critical".into(),
            message: "private fixture details".into(),
            active: true,
            sustain_seconds: 0.0,
        };
        database.call(|connection| {connection.execute_batch("CREATE FUNCTION reject_alert_fixture_func() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated alert failure'; END; $$; CREATE TRIGGER reject_alert_fixture BEFORE INSERT ON alerts FOR EACH ROW EXECUTE FUNCTION reject_alert_fixture_func();")?; Ok(())}).unwrap();
        assert!(alerts.evaluate(&[condition.clone()]).is_err());
        assert_eq!(count("alert.opened"), 0);
        database
            .call(|connection| {
                connection.execute_batch("DROP TRIGGER reject_alert_fixture ON alerts")?;
                Ok(())
            })
            .unwrap();
        alerts.evaluate(&[condition.clone()]).unwrap();
        alerts.evaluate(&[condition.clone()]).unwrap();
        assert_eq!(
            count("alert.opened"),
            1,
            "unchanged sampler observations duplicated the event"
        );
        condition.active = false;
        alerts.evaluate(&[condition]).unwrap();
        assert_eq!(count("alert.recovered"), 1);
        let payloads = database
            .call(|connection| {
                Ok(connection
                    .prepare("SELECT payload_json FROM owned_events")?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .unwrap()
            .join("\n");
        assert!(!payloads.contains("events@example.invalid"));
        assert!(!payloads.contains("private fixture details"));
        assert!(!payloads.contains("One committed task"));
    }

    #[test]
    fn notification_projection_keeps_only_typed_redacted_event_fields() {
        let access = TelegramEvent::new("user.invited").with("email", "private@example.test");
        let owned = owned_notification(&access).expect("access event");
        let encoded = serde_json::to_string(&owned).unwrap();
        assert!(!encoded.contains("private@example.test"));
        assert!(matches!(
            owned,
            results::OwnedEvent::Other(results::OtherOwnedEvent {
                subject_kind,
                subject_id,
                ..
            }) if subject_kind == "access" && subject_id == "users"
        ));

        let health = owned_alert_event(&AlertEvent {
            kind: "alert.opened",
            alert_key: "private-alert-key".into(),
            alert_kind: "cpu".into(),
            subject_kind: "deployment".into(),
            subject_id: "d1111111111111111".into(),
            severity: Some("warning".into()),
            message: "private diagnostic prose".into(),
        });
        let encoded = serde_json::to_string(&health).unwrap();
        assert!(!encoded.contains("private-alert-key"));
        assert!(!encoded.contains("private diagnostic prose"));
        assert!(matches!(
            health,
            results::OwnedEvent::Health(results::HealthOwnedEvent {
                deployment_id,
                severity,
                ..
            }) if deployment_id.as_deref() == Some("d1111111111111111")
                && severity.as_deref() == Some("warning")
        ));

        let deployment = TelegramEvent::new("deployment.restarted")
            .with("repository_id", "r1111111111111111")
            .with("deployment_id", "d1111111111111111")
            .with("component", "web")
            .with("domain", "private.example.test");
        let owned = owned_notification(&deployment).expect("deployment event");
        let encoded = serde_json::to_string(&owned).unwrap();
        assert!(!encoded.contains("private.example.test"));
        assert!(matches!(
            owned,
            results::OwnedEvent::Deployment(results::DeploymentOwnedEvent { component, .. })
                if component.as_deref() == Some("web")
        ));
    }

    #[test]
    fn composed_telegram_operations_enforce_chat_and_scope_visibility() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1000,'t'),('r2222222222222222','/other','other','t',1000,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/repo','t','t'),('w2222222222222222','r2222222222222222','/other','t','t')", [])?;
                transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1111111111111111','r1111111111111111','w1111111111111111','one','worktree','f','{}','running','t',1,'other','t'),('d2222222222222222','r2222222222222222','w2222222222222222','two','worktree','f','{}','running','t',1,'other','t')", [])?;
                Ok(())
            })
            .expect("fixture");
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database,
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .expect("control plane");
        plane
            .access
            .invite(
                InviteUser {
                    email: "viewer@example.test".into(),
                    administrator: false,
                    grants: vec![InvitationGrant {
                        deployment_id: "d1111111111111111".into(),
                        role: AccessRole::Viewer,
                    }],
                },
                &local(),
            )
            .expect("invite");
        plane
            .access
            .accept_invitation(params::AcceptInvitation {
                email: "viewer@example.test".into(),
                subject: Some("subject".into()),
                display_name: None,
            })
            .expect("accept");
        let code = plane.telegram.issue_link_code(7, "viewer").expect("code");
        plane
            .execute(
                "telegram.link",
                serde_json::json!({"code":code}),
                &public("viewer@example.test"),
            )
            .expect("link own chat");
        plane
            .execute(
                "telegram.subscribe",
                serde_json::json!({"chat_id":7,"scope":"deployment:d1111111111111111"}),
                &public("viewer@example.test"),
            )
            .expect("deployment subscription");
        plane
            .execute(
                "telegram.subscribe",
                serde_json::json!({"chat_id":7,"scope":"repository:r1111111111111111"}),
                &public("viewer@example.test"),
            )
            .expect("repository subscription");
        for scope in [
            "server",
            "deployment:d2222222222222222",
            "repository:r2222222222222222",
        ] {
            let error = plane
                .execute(
                    "telegram.subscribe",
                    serde_json::json!({"chat_id":7,"scope":scope}),
                    &public("viewer@example.test"),
                )
                .expect_err("scope denied");
            assert_eq!(error.code, ErrorCode::PermissionDenied);
        }
        let listing = plane
            .execute(
                "telegram.list",
                serde_json::json!({}),
                &public("viewer@example.test"),
            )
            .expect("listing");
        assert_eq!(listing["chats"].as_array().map(Vec::len), Some(1));
        assert_eq!(listing["chats"][0]["chat_id"], 7);
        let denied = plane
            .execute(
                "telegram.unsubscribe",
                serde_json::json!({"chat_id":8,"scope":"server"}),
                &public("viewer@example.test"),
            )
            .expect_err("other chat denied");
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
    }
}
