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
    "ping",
    "config.get",
    "config.env.set",
    "config.reload",
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
    "deployment.list",
    "deployment.status",
    "deployment.apply",
    "deployment.rollback",
    "deployment.start",
    "deployment.stop",
    "deployment.restart",
    "deployment.logs",
    "deployment.remove",
    "deployment.set_domain",
    "test.start",
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
    "health.repositories",
    "health.repository",
    "health.history",
    "health.containers",
    "health.container_remove",
    "usage.repositories",
    "usage.repository",
    "progress.repositories",
    "progress.repository",
    "telegram.link",
    "telegram.subscribe",
    "telegram.unsubscribe",
    "telegram.list",
    "event.wait",
    "plan.overview",
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
    deployments: Deployments,
    events: EventService,
    capacity: CapacityBroker,
    health: HealthService,
    usage: UsageService,
    progress: ProgressService,
    telegram: TelegramService,
    clock: Arc<dyn Clock>,
}

impl ControlPlane {
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
        let deployments = Deployments::with_clock(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::clone(&clock),
        );
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
        let capacity = CapacityBroker::new(database.clone(), config.capacity_socket_path())?;
        let health = HealthService::with_clock(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::clone(&clock),
        );
        let usage = UsageService::with_clock(
            config.clone(),
            database.clone(),
            registry.clone(),
            Arc::clone(&clock),
        );
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
            deployments,
            events,
            capacity,
            health,
            usage,
            progress,
            telegram,
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

    fn dispatch_authorized(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        let now = self.timestamp()?;
        let actor = caller.actor();
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
            "deployment.apply" => {
                let params: params::DeploymentReference = decode(params)?;
                let result = self.deployments.apply(
                    params.path.as_deref(),
                    params.name.as_deref(),
                    params.deployment_id.as_deref(),
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
            "plan.overview" => {
                let params: params::PlanReference = decode(params)?;
                if params.path.is_none() && params.repository_id.is_none() {
                    return encode(self.plan.overview(None)?);
                }
                let repository = self.resolve_repository(
                    params.path.as_deref(),
                    params.repository_id.as_deref(),
                    caller,
                    true,
                )?;
                encode(self.plan.overview(Some(&repository.repository_id))?)
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
        authorization.apply_result(result)
    }

    fn defer(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Option<Result<crate::daemon::DeferredOperation, ProtocolError>> {
        if operation != "event.wait" {
            return None;
        }
        Some((|| {
            let authorization = self.access.authorize(operation, &params, caller)?;
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

struct RepositoryIdentity {
    repository_id: String,
    display_name: String,
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(params).map_err(|error| {
        ProtocolError::new(ErrorCode::ParamsInvalid, "operation parameters are invalid")
            .with_detail(error.to_string())
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
            pid: 1,
            uid: 1000,
            gid: 1000,
            client_kind: devcoordinator2_api::ClientKind::Codex,
            client_session: Some("fixture".into()),
            work: None,
            identity: None,
        }
    }

    fn public(identity: &str) -> Caller {
        Caller {
            pid: 2,
            uid: 999,
            gid: 999,
            client_kind: devcoordinator2_api::ClientKind::Edge,
            client_session: None,
            work: None,
            identity: Some(identity.into()),
        }
    }

    #[test]
    fn composed_dispatch_authorizes_persists_and_publishes_typed_planning_results() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1000,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/repo','t','t')", [])?;
                Ok(())
            })
            .expect("fixture");
        let plane = ControlPlane::with_adapters(
            config(temporary.path()),
            database.clone(),
            Arc::new(|_: &crate::access::RouteAccessSection| Ok(())),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
        )
        .expect("control plane");

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

        let invalid_start = plane
            .execute("test.start", serde_json::json!({"path":"/repo"}), &local())
            .expect_err("invalid repository is rejected by the installed lifecycle");
        assert_eq!(invalid_start.code, ErrorCode::RepositoryNotFound);
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
