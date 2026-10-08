//! Composed protocol-v2 operation dispatcher.
//!
//! Domain modules stay reusable and transport-free. This layer performs the
//! one authorization decision, decodes the registry's typed input, invokes the
//! owning service, filters public collections, and serializes the typed result.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use devcoordinator2_api::params;
use devcoordinator2_api::results;
use devcoordinator2_api::{EmptyParams, ErrorCode, PingData, ProtocolError};
use devcoordinator2_executor_core::log_query::LogQueryOperation;
use rusqlite::OptionalExtension;
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
use crate::plan::{PlanService, SqliteDeploymentEvidence};
use crate::platform::{Clock, HostClock};
use crate::progress::ProgressService;
use crate::repository::Registry;
use crate::routes::RouteFilePublisher;
use crate::server::ServerService;
use crate::sketches::SketchService;
use crate::telegram::{TelegramEvent, TelegramScope, TelegramService, parse_scope};
use crate::test_artifacts::TestArtifactService;
use crate::test_evidence::TestEvidenceService;
use crate::test_lifecycle::{TestLifecycle, TestLifecycleEvent};
use crate::test_logs::TestLogService;
use crate::test_state::ActiveTestArchiveBlocker;
use crate::usage::UsageService;
use crate::{DATABASE_SCHEMA_VERSION, SOURCE_COMMIT};

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// Operations whose domain implementations are installed in this migration
/// checkpoint. Other registered operations fail visibly until their owning
/// service is ported; they never report synthetic success.
pub const FOUNDATION_OPERATIONS: &[&str] = &[
    "storage.inventory",
    "storage.artifact.get",
    "storage.scan",
    "storage.register",
    "storage.legacy.register",
    "storage.protection.set",
    "storage.policy.get",
    "storage.policy.set",
    "storage.roots.list",
    "storage.roots.set",
    "storage.cleanup.plan",
    "storage.cleanup.start",
    "storage.job.status",
    "storage.job.cancel",
    "storage.history",
    "storage.lease.set",
    "storage.lease.release",
    "ping",
    "ticket.request",
    "ticket.settings",
    "ticket.configure",
    "ticket.remote",
    "config.get",
    "config.env.set",
    "config.reload",
    "agent.settings.get",
    "agent.settings.save",
    "agent.roles.save",
    "agent.capabilities.report",
    "agent.instruction.current",
    "deployment.preflight",
    "user.whoami",
    "user.accept_invitation",
    "user.list",
    "user.invite",
    "user.remove",
    "grant.set",
    "grant.remove",
    "repository.register",
    "repository.list",
    "repository.status",
    "repository.presentation.update",
    "repository.archive",
    "repository.unarchive",
    "server.register",
    "server.list",
    "server.stop",
    "deployment.list",
    "deployment.status",
    "deployment.wait",
    "deployment.apply",
    "deployment.rollback",
    "deployment.start",
    "deployment.stop",
    "deployment.restart",
    "deployment.logs",
    "deployment.remove",
    "deployment.set_domain",
    "test.start",
    "test.admission.status",
    "test.admission.wait",
    "test.wait",
    "test.retry",
    "test.status",
    "test.history",
    "test.stop",
    "test.list",
    "test.evidence.get",
    "test.evidence.lookup",
    "test.evidence.image",
    "test.evidence.feedback.create",
    "test.evidence.feedback.reply",
    "test.evidence.feedback.edit",
    "test.evidence.feedback.state",
    "test.evidence.feedback.delete",
    "test.log.catalog",
    "test.log.tail",
    "test.log.search",
    "test.log.range",
    "test.log.failure_context",
    "test.log.retention.get",
    "test.log.retention.set",
    "test.artifact.catalog",
    "test.artifact.file",
    "test.capacity.get",
    "test.capacity.set",
    "health.summary",
    "health.incidents",
    "health.incident.update",
    "health.repositories",
    "health.repository",
    "health.history",
    "health.containers",
    "health.container_remove",
    "usage.repositories",
    "usage.repository",
    "usage.rate_card.list",
    "usage.rate_card.set",
    "progress.repositories",
    "progress.repository",
    "telegram.link",
    "telegram.subscribe",
    "telegram.unsubscribe",
    "telegram.list",
    "event.wait",
    "design.sketch.publish",
    "design.sketch.list",
    "design.sketch.search",
    "design.sketch.story",
    "design.sketch.resolve",
    "design.sketch.get",
    "design.sketch.image",
    "design.sketch.record",
    "design.sketch.decision",
    "design.sketch.activate",
    "design.sketch.description",
    "design.sketch.annotation.create",
    "agent.message.poll",
    "agent.message.claim",
    "agent.message.ack",
    "plan.overview",
    "completion.check",
    "plan.recovery",
    "deployment.recovery",
    "glossary.list",
    "glossary.resolve",
    "glossary.get",
    "glossary.save",
    "glossary.configure",
    "glossary.inherit",
    "glossary.history",
    "glossary.check",
    "glossary.impact",
    "task.history",
    "task.search",
    "task.create",
    "task.update",
    "release.create",
    "release.update",
    "release.request",
    "release.deliver",
    "release.deliver_evidence",
    "release.evidence_show",
    "release.evidence",
    "performance.overview",
    "performance.reviews",
    "performance.review",
    "review.delivery.pending",
    "review.policy.set",
    "review.policy.status",
    "review.delivery.register",
    "review.prepare",
    "review.record",
    "review.show",
    "review.receipt",
    "decision.tail",
    "decision.search",
    "decision.record",
    "decision.summarize",
    "bug.report",
    "bug.list",
    "bug.close",
];

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
    clock: Arc<dyn Clock>,
}

impl ControlPlane {
    pub fn storage(&self) -> &crate::storage::StorageService {
        &self.storage
    }

    pub fn new(config: Config, database: Database) -> Result<Self, ProtocolError> {
        let publisher = Arc::new(RouteFilePublisher::new(
            database.clone(),
            config.routes_path(),
            config.base_domain.clone(),
        ));
        let plane = Self::with_adapters(config, database, publisher, Arc::new(HostClock))?;
        if let Err(error) = plane.deployments.reconcile_routes() {
            tracing::error!(code=%error.code, "route recovery incomplete");
        }
        Ok(plane)
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
        let evidence = Arc::new(SqliteDeploymentEvidence::new(
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
        let telegram = TelegramService::from_config(&config, database.clone());
        let agent_routing = crate::agent_routing::AgentRoutingService::new(database.clone())?;
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

    pub fn expire_previews(&self) -> Result<Vec<results::DeploymentStatus>, ProtocolError> {
        let expired = self.deployments.expire_previews()?;
        for deployment in &expired {
            self.notify(deployment_event("preview.expired", deployment, None));
        }
        Ok(expired)
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

    fn dispatch_authorized(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let now = self.timestamp()?;
        let actor = caller.actor();
        if operation.starts_with("storage.") {
            return match operation {
                "storage.inventory" => encode(self.storage.inventory(decode(params)?)?),
                "storage.artifact.get" => {
                    let p: devcoordinator2_api::storage::Reference = decode(params)?;
                    encode(self.storage.artifact(&p.artifact_id)?)
                }
                "storage.scan" => encode(self.storage.scan(decode(params)?, caller)?),
                "storage.register" => encode(self.storage.register(decode(params)?, caller)?),
                "storage.legacy.register" => {
                    encode(self.storage.register_legacy(decode(params)?, caller)?)
                }
                "storage.protection.set" => encode(self.storage.protect(decode(params)?, caller)?),
                "storage.policy.get" => encode(self.storage.policy(decode(params)?)?),
                "storage.policy.set" => encode(self.storage.set_policy(decode(params)?, caller)?),
                "storage.roots.list" => encode(self.storage.roots()?),
                "storage.roots.set" => encode(self.storage.set_root(decode(params)?, caller)?),
                "storage.cleanup.plan" => encode(self.storage.plan(decode(params)?, caller)?),
                "storage.cleanup.start" => encode(self.storage.start(decode(params)?, caller)?),
                "storage.job.status" => {
                    let p: devcoordinator2_api::storage::JobReference = decode(params)?;
                    encode(self.storage.job(&p.job_id)?)
                }
                "storage.job.cancel" => encode(self.storage.cancel(decode(params)?, caller)?),
                "storage.history" => encode(self.storage.history(decode(params)?)?),
                "storage.lease.set" => encode(self.storage.lease(decode(params)?, caller)?),
                "storage.lease.release" => {
                    encode(self.storage.release_lease(decode(params)?, caller)?)
                }
                _ => Err(ProtocolError::new(
                    ErrorCode::OperationUnknown,
                    "unknown storage operation",
                )),
            };
        }
        if operation.starts_with("ticket.") {
            let principal = self.access.principal(caller)?;
            let administrator = principal.local || principal.administrator;
            let tickets = self.tickets.as_ref().ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ConfigurationInvalid,
                    "configure DEVCOORDINATOR2_BASE_DOMAIN to use feature-request tickets",
                )
            })?;
            return match operation {
                "ticket.request" => encode(tickets.request(decode(params)?, administrator)?),
                "ticket.settings" => encode(tickets.settings(administrator)?),
                "ticket.configure" => encode(tickets.configure(decode(params)?)?),
                "ticket.remote" => encode(tickets.remote(decode(params)?)?),
                _ => Err(ProtocolError::new(
                    ErrorCode::OperationUnknown,
                    "unknown ticket operation",
                )),
            };
        }
        if operation.starts_with("glossary.") {
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
            let repository = repository
                .as_ref()
                .map(|repository| repository.repository_id.as_str());
            return match operation {
                "glossary.list" | "glossary.resolve" => {
                    encode(self.glossary.list(repository, decode(params)?)?)
                }
                "glossary.get" => encode(self.glossary.get(repository, decode(params)?)?),
                "glossary.save" => {
                    encode(
                        self.glossary
                            .save(repository, decode(params)?, &actor, &now)?,
                    )
                }
                "glossary.configure" => {
                    encode(
                        self.glossary
                            .configure(repository, decode(params)?, &actor, &now)?,
                    )
                }
                "glossary.inherit" => {
                    encode(
                        self.glossary
                            .inherit(repository, decode(params)?, &actor, &now)?,
                    )
                }
                "glossary.history" => encode(self.glossary.history(repository, decode(params)?)?),
                "glossary.check" => encode(self.glossary.check(repository, decode(params)?)?),
                "glossary.impact" => encode(self.glossary.impact(decode(params)?)?),
                _ => Err(ProtocolError::new(
                    ErrorCode::OperationUnknown,
                    "Unknown glossary operation",
                )),
            };
        }
        match operation {
            "ping" => {
                let _: EmptyParams = decode(params)?;
                encode(PingData {
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: devcoordinator2_api::PROTOCOL_VERSION,
                    schema_version: DATABASE_SCHEMA_VERSION,
                    executor_schema: devcoordinator2_executor_protocol::EXECUTOR_SCHEMA,
                    source_commit: SOURCE_COMMIT.to_owned(),
                    socket: self.config.socket_path.display().to_string(),
                })
            }
            "agent.settings.get" => {
                let p: devcoordinator2_api::params::AgentSettingsGet = decode(params)?;
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
                    .ensure_capability(&self.config, p.harness, now_ms)?;
                encode(
                    self.agent_routing
                        .settings(p.scope, p.repository_id, p.harness, now_ms)?,
                )
            }
            "agent.settings.save" => {
                let p: devcoordinator2_api::params::AgentSettingsSave = decode(params)?;
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
                encode(self.agent_routing.save_settings(p, &actor, now_ms)?)
            }
            "agent.roles.save" => {
                let p: devcoordinator2_api::params::AgentRolesSave = decode(params)?;
                let (revision, roles) = self.agent_routing.save_roles(p, &actor)?;
                encode(results::AgentRolesSaved { revision, roles })
            }
            "agent.capabilities.report" => {
                let p: devcoordinator2_api::params::AgentCapabilitiesReport = decode(params)?;
                if !caller.is_local() {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "capability reports are accepted from local harnesses only",
                    ));
                }
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                encode(self.agent_routing.report_capabilities(
                    p,
                    caller.client_kind,
                    &actor,
                    now_ms,
                )?)
            }
            "agent.instruction.current" => {
                let p: devcoordinator2_api::params::AgentInstructionCurrent = decode(params)?;
                let now_ms = (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64;
                self.agent_routing
                    .ensure_capability(&self.config, caller.client_kind, now_ms)?;
                encode(self.agent_routing.instruction(
                    p.repository_id,
                    caller.client_kind,
                    caller.model.clone(),
                    caller.effort.clone(),
                    now_ms,
                )?)
            }
            "config.get" => {
                let _: EmptyParams = decode(params)?;
                encode(self.deployments.configuration().get()?)
            }
            "config.reload" => {
                let params: devcoordinator2_api::configuration::Revision = decode(params)?;
                let actor = caller
                    .identity
                    .clone()
                    .unwrap_or_else(|| format!("uid:{}", caller.uid));
                encode(
                    self.deployments
                        .configuration()
                        .reload(&params.expected_revision, &actor)?,
                )
            }
            "config.env.set" => encode(
                self.deployments
                    .set_compose_authorization(decode(params)?, caller)?,
            ),
            "deployment.preflight" => {
                let params: params::DeploymentReference = decode(params)?;
                encode(self.deployments.preflight(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    caller,
                )?)
            }
            "user.whoami" => {
                let _: params::Empty = decode(params)?;
                encode(self.access.who_am_i(caller)?)
            }
            "user.accept_invitation" => encode(self.access.accept_invitation(decode(params)?)?),
            "user.list" => {
                let _: params::Empty = decode(params)?;
                encode(self.access.list_users()?)
            }
            "user.invite" => {
                let result = self.access.invite(decode(params)?, caller)?;
                self.notify(TelegramEvent::new("user.invited").with("email", result.email.clone()));
                encode(result)
            }
            "user.remove" => {
                let result = self.access.remove_user(decode(params)?, caller)?;
                self.notify(TelegramEvent::new("user.removed").with("email", result.email.clone()));
                encode(result)
            }
            "grant.set" => {
                let result = self.access.set_grant(decode(params)?, caller)?;
                self.notify(
                    TelegramEvent::new("grant.set")
                        .with("email", result.email.clone())
                        .with("deployment_id", result.deployment_id.clone()),
                );
                encode(result)
            }
            "grant.remove" => {
                let result = self.access.remove_grant(decode(params)?, caller)?;
                self.notify(
                    TelegramEvent::new("grant.removed")
                        .with("email", result.email.clone())
                        .with("deployment_id", result.deployment_id.clone()),
                );
                encode(result)
            }
            "repository.register" => {
                let params: params::PathOnly = decode(params)?;
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
                encode(result)
            }
            "repository.list" => {
                let params: params::RepositoryList = decode(params)?;
                encode(self.registry.list_repositories(params.include_archived)?)
            }
            "repository.status" => {
                let params: params::PathOnly = decode(params)?;
                let mut result = self
                    .registry
                    .repository_status(Path::new(&params.path), Some((caller.uid, caller.gid)))?;
                result.current_test = self.tests.current_summary_ref(&params.path, caller);
                encode(result)
            }
            "repository.presentation.update" => {
                let params: params::RepositoryPresentationUpdate = decode(params)?;
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
                encode(result)
            }
            "repository.archive" => {
                let params: params::ArchiveRepository = decode(params)?;
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
                encode(result)
            }
            "repository.unarchive" => {
                let params: params::UnarchiveRepository = decode(params)?;
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
                encode(result)
            }
            "server.register" => encode(self.servers.register(decode(params)?)?),
            "server.list" => encode(self.servers.list(decode(params)?)?),
            "server.stop" => encode(self.servers.stop(decode(params)?)?),
            "deployment.list" => encode(self.deployments.list(decode(params)?, caller)?),
            "deployment.status" => {
                let params: params::DeploymentReference = decode(params)?;
                encode(self.deployments.status(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    caller,
                )?)
            }
            "test.admission.status" => {
                let _: params::TestAdmissionStatus = decode(params)?;
                encode(self.tests.admission_status()?)
            }
            "deployment.apply" => {
                let params: params::DeploymentApply = decode(params)?;
                let result = self.deployments.apply_with_candidate(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.candidate.as_ref(),
                    caller,
                )?;
                self.health.sampler().request_storage();
                self.notify(deployment_event("deployment.applied", &result, None));
                encode(result)
            }
            "deployment.rollback" => {
                let params: params::DeploymentReference = decode(params)?;
                let result = self.deployments.rollback(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    caller,
                )?;
                self.health.sampler().request_storage();
                self.notify(deployment_event("deployment.rolled_back", &result, None));
                encode(result)
            }
            "deployment.logs" => {
                let params: params::DeploymentLogs = decode(params)?;
                encode(self.deployments.logs(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    &params.component,
                    params.tail_lines,
                    caller,
                )?)
            }
            "deployment.start" | "deployment.stop" | "deployment.restart" => {
                let action = operation.split('.').nth(1).unwrap_or_default();
                let params: params::DeploymentControl = decode(params)?;
                let result = self.deployments.control(
                    action,
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
                    params.component.as_deref(),
                    caller,
                )?;
                self.notify(deployment_event(operation, &result, params.component));
                encode(result)
            }
            "deployment.remove" => {
                let params: params::RemoveDeployment = decode(params)?;
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
                encode(result)
            }
            "deployment.set_domain" => {
                let params: params::SetDomain = decode(params)?;
                let deployment_id = params.deployment_id.clone();
                let result = self.deployments.set_domain(params)?;
                self.notify(
                    TelegramEvent::new("deployment.domain_changed")
                        .with("deployment_id", deployment_id)
                        .with("domain", result.domain.clone()),
                );
                encode(result)
            }
            "test.start" => {
                let result = self.tests.start(decode(params)?, caller)?;
                encode(result)
            }
            "test.retry" => {
                let result = self.tests.retry(decode(params)?, caller)?;
                encode(result)
            }
            "test.status" => {
                let params: params::PathOnly = decode(params)?;
                encode(self.tests.status(&params.path, caller)?)
            }
            "test.stop" => {
                let params: params::StopTest = decode(params)?;
                encode(self.tests.stop(&params.path, params.reason, caller)?)
            }
            "test.history" => encode(self.tests.history(decode(params)?, caller)?),
            "test.list" => {
                let params: params::TestList = decode(params)?;
                let mut result = self.tests.list_current_page(params)?;
                self.test_evidence.enrich_list(&mut result);
                encode(result)
            }
            "test.evidence.get" => encode(self.test_evidence.get(decode(params)?, caller)?),
            "test.evidence.lookup" => encode(self.test_evidence.lookup(decode(params)?, caller)?),
            "test.evidence.image" => encode(self.test_evidence.image(decode(params)?, caller)?),
            "test.evidence.feedback.create" => {
                let params: params::CreateFeedback = decode(params)?;
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
                encode(result)
            }
            "test.evidence.feedback.reply" => {
                let params: params::FeedbackReply = decode(params)?;
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
                encode(result)
            }
            "test.evidence.feedback.edit" => {
                let params: params::FeedbackEdit = decode(params)?;
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
                encode(result)
            }
            "test.evidence.feedback.state" => {
                let params: params::FeedbackStateChange = decode(params)?;
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
                encode(result)
            }
            "test.evidence.feedback.delete" => {
                let params: params::FeedbackDelete = decode(params)?;
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
                encode(result)
            }
            "design.sketch.publish" => {
                let params: params::SketchPublish = decode(params)?;
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
                encode(result)
            }
            "design.sketch.list" => encode(self.sketches.list(decode(params)?)?),
            "design.sketch.search" => encode(self.sketches.search(decode(params)?)?),
            "design.sketch.story" => encode(self.sketches.story(decode(params)?)?),
            "design.sketch.resolve" => encode(self.sketches.resolve(decode(params)?)?),
            "design.sketch.get" => encode(self.sketches.get(decode(params)?, &caller.actor())?),
            "design.sketch.image" => encode(self.sketches.image(decode(params)?)?),
            "design.sketch.record" => encode(self.sketches.record(decode(params)?)?),
            "design.sketch.decision" => {
                let params: params::SketchDecisionChange = decode(params)?;
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
                encode(result)
            }
            "design.sketch.activate" => {
                let params: params::SketchActivate = decode(params)?;
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
                encode(result)
            }
            "design.sketch.description" => {
                let params: params::SketchDescriptionChange = decode(params)?;
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
                encode(result)
            }
            "design.sketch.annotation.create" => {
                let params: params::SketchAnnotationCreate = decode(params)?;
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
                encode(result)
            }
            "agent.message.poll" => encode(self.sketches.message_poll(decode(params)?)?),
            "agent.message.claim" => encode(self.sketches.message_claim(decode(params)?, caller)?),
            "agent.message.ack" => encode(self.sketches.message_ack(decode(params)?, caller)?),
            "test.log.catalog" => {
                let params: params::LogCatalog = decode(params)?;
                self.logs.query(
                    LogQueryOperation::Catalog,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )
            }
            "test.log.tail" => {
                let params: params::LogTail = decode(params)?;
                self.logs.query(
                    LogQueryOperation::Tail,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )
            }
            "test.log.search" => {
                let params: params::LogSearch = decode(params)?;
                self.logs.query(
                    LogQueryOperation::Search,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )
            }
            "test.log.range" => {
                let params: params::LogRange = decode(params)?;
                self.logs.query(
                    LogQueryOperation::Range,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )
            }
            "test.log.failure_context" => {
                let params: params::LogFailureContext = decode(params)?;
                self.logs.query(
                    LogQueryOperation::FailureContext,
                    Path::new(&params.path),
                    encode(&params)?,
                    caller,
                )
            }
            "test.log.retention.get" => {
                let _: params::Empty = decode(params)?;
                encode(self.logs.retention()?)
            }
            "test.log.retention.set" => {
                let params: params::SetRetention = decode(params)?;
                encode(self.logs.set_retention(
                    params.max_age_seconds,
                    params.case_depth,
                    &actor,
                    &now,
                )?)
            }
            "test.artifact.catalog" => encode(self.artifacts.catalog(decode(params)?, caller)?),
            "test.artifact.file" => encode(self.artifacts.file(decode(params)?, caller)?),
            "test.capacity.get" => {
                let _: params::Empty = decode(params)?;
                encode(self.capacity.snapshot()?)
            }
            "test.capacity.set" => {
                let params: params::SetCapacity = decode(params)?;
                let cap = match params.cap {
                    params::CapacityCap::Value(cap) => Some(u32::from(cap)),
                    params::CapacityCap::Null => None,
                };
                encode(self.capacity.set_cap(cap, &actor)?)
            }
            "health.summary" => {
                let _: params::Empty = decode(params)?;
                encode(self.health.summary()?)
            }
            "health.incidents" => encode(self.incidents.list(decode(params)?)?),
            "health.incident.update" => {
                let incident = self.incidents.update(decode(params)?, &caller.actor())?;
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
                encode(incident)
            }
            "health.repositories" => {
                let _: params::Empty = decode(params)?;
                encode(self.health.repositories()?)
            }
            "health.repository" => {
                let params: params::PathOnly = decode(params)?;
                encode(self.health.repository(&params.path, caller)?)
            }
            "health.history" => encode(self.health.history(decode(params)?)?),
            "health.containers" => {
                let _: params::Empty = decode(params)?;
                encode(self.health.containers()?)
            }
            "health.container_remove" => {
                let params: params::RemoveContainer = decode(params)?;
                encode(self.health.remove_container(&params.container_id)?)
            }
            "usage.repositories" => encode(self.usage.repositories(decode(params)?)?),
            "usage.repository" => encode(self.usage.repository(decode(params)?)?),
            "usage.rate_card.list" => encode(self.rate_cards.list(decode(params)?)?),
            "usage.rate_card.set" => encode(self.rate_cards.set(decode(params)?, &actor, &now)?),
            "progress.repositories" => {
                let _: params::Empty = decode(params)?;
                encode(self.progress.repositories()?)
            }
            "progress.repository" => encode(self.progress.repository(decode(params)?)?),
            "telegram.link" => {
                let params: params::TelegramLink = decode(params)?;
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
                encode(self.telegram.link(&params.code, &email)?)
            }
            "telegram.subscribe" => {
                let params: params::TelegramSubscription = decode(params)?;
                self.authorize_telegram_chat(caller, params.chat_id)?;
                self.authorize_telegram_scope(caller, &params.scope)?;
                encode(self.telegram.subscribe(params)?)
            }
            "telegram.unsubscribe" => {
                let params: params::TelegramSubscription = decode(params)?;
                self.authorize_telegram_chat(caller, params.chat_id)?;
                encode(self.telegram.unsubscribe(params)?)
            }
            "telegram.list" => {
                let _: params::Empty = decode(params)?;
                let principal = self.access.principal(caller)?;
                let email = (!principal.local && !principal.administrator)
                    .then_some(principal.identity)
                    .flatten();
                encode(self.telegram.list(email.as_deref())?)
            }
            "deployment.recovery" => {
                let request: devcoordinator2_api::runtime_recovery::Request = decode(params)?;
                self.resolve_repository(None, Some(&request.repository_id), caller, false)?;
                encode(
                    self.deployments
                        .recover_saved_preview(request, caller, &actor, &now)?,
                )
            }
            "plan.recovery" => {
                let request: devcoordinator2_api::recovery::Request = decode(params)?;
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
                encode(result)
            }
            "plan.overview" => {
                let params: params::PlanReference = decode(params)?;
                if params.path.is_none() && params.repository_id.is_none() {
                    return encode(self.plan.overview_page(
                        None,
                        params.offset,
                        params.expected_revision.as_deref(),
                    )?);
                }
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                encode(self.plan.overview_page(
                    Some(&repository.repository_id),
                    params.offset,
                    params.expected_revision.as_deref(),
                )?)
            }
            "completion.check" => {
                let params: params::CompletionCheck = decode(params)?;
                let repository = self.resolve_repository(Some(&params.path), None, caller, true)?;
                let result = crate::completion::CompletionService::new(self.database.clone())
                    .check_path(
                        &repository.repository_id,
                        Path::new(&params.path),
                        params.manifest,
                    )?;
                encode(result)
            }
            "task.search" => {
                let params: params::TaskSearch = decode(params)?;
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                encode(self.plan.search_tasks(&repository.repository_id, params)?)
            }
            "task.history" => {
                let params: params::TaskHistory = decode(params)?;
                encode(self.plan.task_history(&params.task_id)?)
            }
            "task.create" => {
                let params: params::TaskCreate = decode(params)?;
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
                encode(result)
            }
            "task.update" => {
                let result = self.plan.update_task(decode(params)?, &actor, &now)?;
                self.publish_planning(
                    "task.updated",
                    &result.repository_id,
                    "task",
                    &result.task_id,
                    enum_text(&result.status),
                );
                encode(result)
            }
            "release.create" => {
                let params: params::ReleaseCreate = decode(params)?;
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
                encode(result)
            }
            "release.update" => {
                let result = self.plan.update_release(decode(params)?, &actor, &now)?;
                if let Some(repository_id) = &result.repository_id {
                    self.publish_planning(
                        "release.updated",
                        repository_id,
                        "release",
                        &result.release_id,
                        enum_text(&result.status),
                    );
                }
                encode(result)
            }
            "release.request" => {
                let params: params::ReleaseRequest = decode(params)?;
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
                encode(result)
            }
            "release.deliver" => {
                let params: params::ReleaseDeliver = decode(params)?;
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
                encode(result)
            }
            "decision.tail" => {
                let params: params::DecisionTail = decode(params)?;
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                encode(self.plan.decision_tail(&repository.repository_id, params)?)
            }
            "performance.overview" => encode(
                self.reviews.performance_overview(
                    decode(params)?,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                )?,
            ),
            "performance.reviews" => encode(self.reviews.performance_reviews(decode(params)?)?),
            "performance.review" => encode(
                self.reviews.performance_review(
                    decode(params)?,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                )?,
            ),
            "review.delivery.pending" => {
                self.deliver_review_reminders()?;
                encode(self.reviews.delivery_pending(
                    decode(params)?,
                    (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
                )?)
            }
            "review.policy.set" => encode(self.reviews.policy_set(
                decode(params)?,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
            )?),
            "review.policy.status" => encode(self.reviews.policy_status(
                decode(params)?,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
            )?),
            "review.delivery.register" => encode(self.reviews.delivery_register(
                decode(params)?,
                (self.clock.now_utc().unix_timestamp_nanos() / 1_000_000) as u64,
                crate::review::RegistrationMode::Replace,
            )?),
            "review.prepare" => encode(
                self.reviews.prepare(
                    decode(params)?,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                )?,
            ),
            "review.record" => encode(
                self.reviews.record(
                    decode(params)?,
                    &actor,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                )?,
            ),
            "review.show" => encode(self.reviews.show(decode(params)?)?),
            "review.receipt" => encode(self.reviews.receipt(decode(params)?)?),
            "release.evidence" => encode(self.deliveries.receipt(decode(params)?)?),
            "release.deliver_evidence" => encode(
                self.deliveries.deliver(
                    decode(params)?,
                    caller,
                    &actor,
                    self.clock
                        .now_utc()
                        .unix_timestamp_nanos()
                        .div_euclid(1_000_000) as u64,
                )?,
            ),
            "release.evidence_show" => encode(self.deliveries.show(decode(params)?)?),
            "decision.search" => {
                let params: params::DecisionSearch = decode(params)?;
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                encode(
                    self.plan
                        .search_decisions(&repository.repository_id, params)?,
                )
            }
            "decision.record" => {
                let params: params::DecisionRecord = decode(params)?;
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
                encode(result)
            }
            "decision.summarize" => {
                let params: params::DecisionSummarize = decode(params)?;
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
                encode(result)
            }
            "bug.report" => {
                let params: params::BugReport = decode(params)?;
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
                encode(result)
            }
            "bug.list" => {
                let _: params::Empty = decode(params)?;
                encode(results::BugList {
                    bugs: bugs::list_open(&self.config.bugs_dir).map_err(bug_error)?,
                    // The backing host path is deliberately private.
                    store: None,
                })
            }
            "bug.close" => {
                let params: params::BugClose = decode(params)?;
                let result =
                    bugs::close(&self.config.bugs_dir, &params.bug_id).map_err(bug_error)?;
                self.notify(
                    TelegramEvent::new("bug.closed")
                        .with("component", result.component.clone())
                        .with("summary", result.summary.clone()),
                );
                encode(result)
            }
            _ => Err(ProtocolError::new(
                ErrorCode::InternalError,
                "operation handler is not installed in this migration checkpoint",
            )),
        }
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

    fn repository_by_id(
        &self,
        repository_id: &str,
        allow_archived: bool,
    ) -> Result<RepositoryIdentity, ProtocolError> {
        let repository_id = repository_id.to_owned();
        let lookup = repository_id.clone();
        let state = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT archived_at,merged_into_repository_id,display_name FROM repositories WHERE repository_id=?1",
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
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT repository_id,name FROM releases WHERE release_id=?1",
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
                let visible = self.database.call(move |connection| {
                    for deployment in deployments {
                        let found = connection.query_row(
                            "SELECT EXISTS(SELECT 1 FROM deployments WHERE deployment_id=?1 AND repository_id=?2) OR EXISTS(SELECT 1 FROM observed_deployments WHERE observed_deployment_id=?1 AND repository_id=?2)",
                            rusqlite::params![deployment, repository_id],
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

impl OperationExecutor for ControlPlane {
    fn execute(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let authorization = self.access.authorize(operation, &params, caller)?;
        let result = self.dispatch_authorized(operation, authorization.params.clone(), caller)?;
        // Only an actual native capability selects the alarm route; client names do not.
        if !caller.via_edge
            && !operation.starts_with("review.delivery.")
            && let Some(work) = caller.work.as_ref().and_then(|work| work.context.as_ref())
            && let Some(capability) = &work.alarm
            && let Ok(repository) = self.resolve_repository(
                authorization.params.get("path").and_then(Value::as_str),
                authorization
                    .params
                    .get("repository_id")
                    .and_then(Value::as_str),
                caller,
                false,
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
            && let Ok(repository) = self.resolve_repository(
                params.get("path").and_then(Value::as_str),
                params.get("repository_id").and_then(Value::as_str),
                caller,
                false,
            )
        {
            if let Err(error) = self.deliver_review_reminders() {
                tracing::warn!(code=%error.code,"review reminders temporarily unavailable");
            }
            if let Ok(messages) = self.sketches.message_poll_kind(
                params::AgentMessagePoll {
                    repository_id: repository.repository_id,
                    after_id: None,
                    limit: 2,
                },
                Some("performance_review.reminder"),
            ) && !messages.messages.is_empty()
                && let Some(object) = result.as_object_mut()
            {
                object.insert("_agent_messages".into(), encode(messages.messages)?);
            }
        }
        Ok(result)
    }

    fn defer(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Option<Result<crate::daemon::DeferredOperation, ProtocolError>> {
        if !matches!(
            operation,
            "event.wait" | "test.admission.wait" | "test.wait" | "deployment.wait"
        ) {
            return None;
        }
        Some((|| {
            let authorization = self.access.authorize(operation, &params, caller)?;
            if operation == "test.admission.wait" {
                let request: params::TestAdmissionWait = decode(authorization.params)?;
                let deadline = parse_deadline(&request.deadline_at)?;
                let runtime = self.tests.admission_runtime_dir();
                let tests = self.tests.clone();
                return Ok(Box::pin(async move {
                    let wait = tokio::time::timeout_at(
                        deadline,
                        crate::test_admission::wait_for_admission_open(runtime),
                    );
                    match wait.await {
                        Ok(Ok(())) => encode(tests.admission_status()?),
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
                    self.resolve_repository(Some(&request.path), None, caller, false)?;
                let events = self.events.clone();
                let access = self.access.clone();
                let wait_caller = caller.clone();
                let tests = self.tests.clone();
                let run_id = request.run_id.clone();
                let path = request.path.clone();
                let deadline = request.deadline_at.clone();
                let status_caller = caller.clone();
                return Ok(Box::pin(async move {
                    loop {
                        let access_for_wait = access.clone();
                        let caller_for_wait = wait_caller.clone();
                        let subscription = events.subscribe_with_visibility(
                            params::EventWait {
                                cursor: None,
                                filters: vec![params::EventFilter {
                                    filter_id: "test-wait".into(),
                                    categories: vec![params::EventCategory::Test],
                                    kinds: vec!["test.finished".into()],
                                    repository_ids: vec![repository.repository_id.clone()],
                                    deployment_ids: vec![],
                                    deadline_at: Some(deadline.clone()),
                                }],
                                limit: 10,
                            },
                            Arc::new(move || access_for_wait.event_visibility(&caller_for_wait)),
                        )?;
                        let result = subscription.receive().await?;
                        if !result.heartbeat_due.is_empty() {
                            let latest = tests.status(&path, &status_caller)?;
                            return Err(wait_deadline_error(
                                "test run did not finish before the requested deadline",
                                Some(format!("{:?}", latest.status)),
                                Some(run_id.clone()),
                                None,
                            ));
                        }
                        let latest = tests.status(&path, &status_caller)?;
                        if latest.run_id == run_id && latest.status != results::TestStatus::Running
                        {
                            return encode(results::TestWaitResult {
                                completed: true,
                                timed_out: false,
                                status: latest,
                            });
                        }
                    }
                }) as crate::daemon::DeferredOperation);
            }
            if operation == "deployment.wait" {
                let request: params::DeploymentWait = decode(authorization.params.clone())?;
                let current = self.deployments.status(
                    request.path.as_deref(),
                    request.name.as_deref(),
                    request.deployment_id.as_deref(),
                    caller,
                )?;
                if deployment_wait_satisfied(&current, request.state) {
                    return Ok(Box::pin(async move {
                        encode(results::DeploymentWaitResult {
                            completed: true,
                            timed_out: false,
                            status: current,
                        })
                    }) as crate::daemon::DeferredOperation);
                }
                let events = self.events.clone();
                let access = self.access.clone();
                let wait_caller = caller.clone();
                let deployments = self.deployments.clone();
                let path = request.path.clone();
                let name = request.name.clone();
                let deployment_id = request.deployment_id.clone();
                let deadline = request.deadline_at.clone();
                let target_deployment_id = current.deployment_id.clone();
                let state = request.state;
                let status_caller = caller.clone();
                return Ok(Box::pin(async move {
                    loop {
                        let access_for_wait = access.clone();
                        let caller_for_wait = wait_caller.clone();
                        let subscription = events.subscribe_with_visibility(
                            params::EventWait {
                                cursor: None,
                                filters: vec![params::EventFilter {
                                    filter_id: "deployment-wait".into(),
                                    categories: vec![params::EventCategory::Deployment],
                                    kinds: vec![
                                        "deployment.applied".into(),
                                        "deployment.failed".into(),
                                        "deployment.start".into(),
                                        "deployment.restart".into(),
                                    ],
                                    repository_ids: vec![],
                                    deployment_ids: vec![target_deployment_id.clone()],
                                    deadline_at: Some(deadline.clone()),
                                }],
                                limit: 10,
                            },
                            Arc::new(move || access_for_wait.event_visibility(&caller_for_wait)),
                        )?;
                        let result = subscription.receive().await?;
                        let latest = deployments.status(
                            path.as_deref(),
                            name.as_deref(),
                            deployment_id.as_deref(),
                            &status_caller,
                        )?;
                        if deployment_wait_satisfied(&latest, state) {
                            return encode(results::DeploymentWaitResult {
                                completed: true,
                                timed_out: false,
                                status: latest,
                            });
                        }
                        if !result.heartbeat_due.is_empty() {
                            return Err(wait_deadline_error(
                                "deployment did not reach the requested state before the deadline",
                                Some(latest.state.clone()),
                                None,
                                Some(latest.deployment_id.clone()),
                            ));
                        }
                    }
                }) as crate::daemon::DeferredOperation);
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

fn parse_deadline(value: &str) -> Result<tokio::time::Instant, ProtocolError> {
    let parsed = time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "deadline_at must be an RFC 3339 timestamp",
            )
        })?;
    let now = time::OffsetDateTime::now_utc();
    let duration = parsed - now;
    Ok(tokio::time::Instant::now() + duration.try_into().unwrap_or_default())
}

fn wait_deadline_error(
    reason: &str,
    state: Option<String>,
    run_id: Option<String>,
    deployment_id: Option<String>,
) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::WaitDeadlineReached,
        "the requested wait deadline was reached",
    )
    .with_recovery(devcoordinator2_api::recovery::Guidance {
        class: devcoordinator2_api::recovery::Class::Transient,
        retryable: false,
        waitable: true,
        safe_to_continue: true,
        state,
        reason: reason.into(),
        run_id,
        deployment_id,
        generation: None,
        repository_id: None,
        options: vec![devcoordinator2_api::recovery::RecoveryOption {
            id: "inspect".into(),
            action: devcoordinator2_api::recovery::Action::Inspect,
            operation: None,
            effect:
                "Read the latest status and choose whether to wait again or continue elsewhere."
                    .into(),
            target: None,
            cost: Some("immediate".into()),
            risk: Some("none".into()),
            prerequisites: vec![],
            independent_work_safe: true,
            user_action_required: false,
        }],
        field_errors: vec![],
        example: None,
    })
}

fn deployment_wait_satisfied(
    status: &results::DeploymentStatus,
    state: params::DeploymentWaitState,
) -> bool {
    match state {
        params::DeploymentWaitState::Applied => {
            status.current_generation.is_some() && status.state != "applying"
        }
        params::DeploymentWaitState::Ready => status.readiness.as_ref().is_some_and(|r| r.ready),
        params::DeploymentWaitState::SourceCurrent => status
            .readiness
            .as_ref()
            .is_some_and(|r| r.pending_apply == Some(false)),
        params::DeploymentWaitState::RouteAcknowledged => {
            status.route_port.is_some() && status.state == "running"
        }
    }
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
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let mut configuration = config(temporary.path());
        configuration.base_domain = "https://example.test/invalid-path".into();
        let error = ControlPlane::new(configuration, database)
            .err()
            .expect("an invalid configured ticket origin must still be rejected");
        assert_eq!(error.code, ErrorCode::ParamsInvalid);
    }

    fn verify_large_plan_pages() {
        use devcoordinator2_api::params::{ReleaseKind, ReleaseStatus};
        use devcoordinator2_api::results::{
            DecisionState, ElaborationRequest, PlanDetail, PlanRelease, PlanTask, PreviewRequest,
        };
        use devcoordinator2_api::{ResponseEnvelope, encode_response};
        use sha2::{Digest, Sha256};

        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let repository_id = "r1111111111111111";
        let mut expected = PlanDetail {
            repository_id: repository_id.into(),
            display_name: "Большой план".into(),
            archived: false,
            merged_into_repository_id: None,
            releases: Vec::new(),
            tasks: Vec::new(),
            tasks_truncated: true,
            elaboration_requests: Vec::new(),
            preview_requested: Vec::new(),
            decisions: DecisionState {
                unsummarized_count: 0,
                summary_due: false,
            },
            revision: String::new(),
            next_offset: None,
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
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES(?1,'/repo',?2,'t',1000,'t')", rusqlite::params![fixture.repository_id, fixture.display_name])?;
            transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111',?1,'/repo','t','t')", [&fixture.repository_id])?;
            for release in fixture.releases {
                transaction.execute("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,note,requested_at,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,'preview','requested',?5,'t','t','uid:1000','t')", rusqlite::params![release.release_id, fixture.repository_id, release.seq, release.name, release.note])?;
            }
            for task in fixture.tasks {
                transaction.execute("INSERT INTO tasks(task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'goal','planned',120,?10,'t','uid:1000','t')", rusqlite::params![task.task_id, fixture.repository_id, task.parent_task_id, task.release_id, task.seq, task.position, task.title, "Результат ".repeat(180), task.impact, task.elaboration_needed])?;
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
        expected.revision = Sha256::digest(serde_json::to_vec(&expected).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
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
        let mut offset = 0;
        let mut pages = 0;
        let mut first_next = None;
        loop {
            let value = plane.execute("plan.overview", serde_json::json!({"repository_id": repository_id, "offset": offset, "expected_revision": (offset != 0).then_some(&expected.revision)}), &local()).expect("page dispatch");
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
            assert_eq!(page.tasks_truncated, expected.tasks_truncated);
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
                first_next = page.next_offset;
            }
            match page.next_offset {
                Some(next) => {
                    assert_eq!(next, offset + count as u32);
                    offset = next;
                }
                None => break,
            }
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
        let total = expected.releases.len()
            + expected.tasks.len()
            + expected.elaboration_requests.len()
            + expected.preview_requested.len();
        for params in [
            serde_json::json!({"repository_id": repository_id, "offset": 1}),
            serde_json::json!({"repository_id": repository_id, "offset": total, "expected_revision": expected.revision}),
            serde_json::json!({"repository_id": repository_id, "offset": u32::MAX, "expected_revision": expected.revision}),
            serde_json::json!({"offset": 1, "expected_revision": expected.revision}),
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
        for offset in [0, first_next.unwrap()] {
            assert_eq!(plane.execute("plan.overview", serde_json::json!({"repository_id": repository_id, "offset": offset, "expected_revision": expected.revision}), &local()).unwrap_err().code, ErrorCode::CursorStale);
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
                    "UPDATE releases SET note=?1 WHERE release_id='v0000000000000001'",
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
            "a plan record exceeds the response page budget"
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
    fn every_foundation_operation_exists_in_the_exhaustive_registry() {
        let implemented = FOUNDATION_OPERATIONS
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let registered = devcoordinator2_api::OPERATIONS
            .iter()
            .map(|operation| operation.name)
            .collect::<HashSet<_>>();
        assert_eq!(implemented, registered);
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
                            "SELECT COUNT(*) FROM owned_events WHERE kind=?1",
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
                "CREATE TRIGGER reject_event_fixture BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'isolated transaction failure'); END;"
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
                    connection.execute_batch("DROP TRIGGER reject_event_fixture")?;
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
        database.call(|connection| {connection.execute_batch("CREATE TRIGGER reject_alert_fixture BEFORE INSERT ON alerts BEGIN SELECT RAISE(ABORT,'isolated alert failure'); END;")?; Ok(())}).unwrap();
        assert!(alerts.evaluate(&[condition.clone()]).is_err());
        assert_eq!(count("alert.opened"), 0);
        database
            .call(|connection| {
                connection.execute_batch("DROP TRIGGER reject_alert_fixture")?;
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
