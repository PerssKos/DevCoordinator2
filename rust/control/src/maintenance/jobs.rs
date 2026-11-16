//! Small resumable units; the scheduler owns their timing and durable cursors.
use super::{Budget, Job, JobResult, JobStatus, PeriodicJob};
use crate::control_plane::ControlPlane;
use devcoordinator2_api::ProtocolError;
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
enum Kind {
    Metrics,
    StorageHealth,
    Partitions,
    Retention,
    Logs,
    Tests,
    Previews,
    Routes,
    Reviews,
    Capabilities,
    Callbacks,
    TelegramPoll,
    TelegramDeliver,
}
struct ServiceJob {
    plane: ControlPlane,
    kind: Kind,
}

impl Job for ServiceJob {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Metrics => "metric-sample",
            Kind::StorageHealth => "storage-health",
            Kind::Partitions => "metric-partitions",
            Kind::Retention => "metric-retention",
            Kind::Logs => "log-retention",
            Kind::Tests => "test-reconcile",
            Kind::Previews => "preview-expiry",
            Kind::Routes => "route-reconcile",
            Kind::Reviews => "review-reminders",
            Kind::Capabilities => "capability-refresh",
            Kind::Callbacks => "hub-callbacks",
            Kind::TelegramPoll => "telegram-poll",
            Kind::TelegramDeliver => "telegram-deliver",
        }
    }
    fn trigger(&self) -> Option<Arc<tokio::sync::Notify>> {
        match self.kind {
            Kind::Tests => Some(self.plane.tests().reconciliation_signal()),
            Kind::Logs => Some(self.plane.logs().maintenance_signal()),
            Kind::TelegramDeliver => Some(self.plane.telegram().delivery_signal()),
            _ => None,
        }
    }
    fn checkpoint(&self) -> Option<String> {
        match self.kind {
            Kind::TelegramPoll => Some(self.plane.telegram().update_cursor()),
            Kind::StorageHealth => Some(self.plane.health().sampler().storage_cursor()),
            _ => None,
        }
    }
    fn priority(&self) -> i16 {
        match self.kind {
            Kind::Metrics | Kind::Reviews | Kind::Tests => 20,
            Kind::Routes | Kind::Previews | Kind::Callbacks => 10,
            _ => 0,
        }
    }
    fn run(&self, budget: &mut Budget, cursor: Option<String>) -> JobResult {
        let run = || -> Result<JobResult, ProtocolError> {
            let done = |next: Option<String>, rows, bytes| JobResult {
                status: next.map_or(JobStatus::Complete, |cursor| JobStatus::Continue { cursor }),
                rows,
                bytes,
            };
            match self.kind {
                Kind::Metrics => {
                    self.plane.health().sampler().tick()?;
                }
                Kind::StorageHealth => {
                    self.plane
                        .health()
                        .sampler()
                        .resume_storage_cursor(cursor.as_deref())?;
                    self.plane.health().sampler().storage_tick()?;
                }
                Kind::Partitions => {
                    let report = self
                        .plane
                        .database()
                        .maintenance_transaction(|c| {
                            Ok(crate::database::maintenance::ensure_metric_partitions(
                                c,
                                time::OffsetDateTime::now_utc(),
                            )?)
                        })
                        .map_err(|_| {
                            ProtocolError::new(
                                devcoordinator2_api::ErrorCode::InternalError,
                                "metric partition maintenance unavailable",
                            )
                        })?;
                    return Ok(done(
                        (!report.complete).then(String::new),
                        u64::from(report.created),
                        0,
                    ));
                }
                Kind::Retention => {
                    let result = self.plane.health().sampler().metrics().expire_step()?;
                    return Ok(done(
                        (!result.complete).then(String::new),
                        result.rows_deleted,
                        result.bytes_selected,
                    ));
                }
                Kind::Logs => {
                    let step =
                        self.plane
                            .logs()
                            .run_maintenance_step(cursor, 1, budget.remaining())?;
                    if !step.report.errors.is_empty() {
                        tracing::warn!(
                            count = step.report.errors.len(),
                            "some log retention entries could not be examined"
                        );
                    }
                    return Ok(done(
                        step.next_worktree_id.filter(|_| !step.complete),
                        step.visited_entries,
                        step.processed_bytes,
                    ));
                }
                Kind::Tests => {
                    let (next, error) = self.plane.tests().reconcile_orphans_step(cursor)?;
                    if let Some(error) = error {
                        tracing::warn!(code=%error.code,"one test reconciliation remains incomplete");
                    }
                    return Ok(done(next, 1, 0));
                }
                Kind::Previews => return Ok(done(self.plane.expire_previews_step(cursor)?, 1, 0)),
                Kind::Routes => {
                    return Ok(done(
                        self.plane.deployments().reconcile_routes_step(cursor)?,
                        1,
                        0,
                    ));
                }
                Kind::Reviews => self.plane.deliver_review_reminders()?,
                Kind::Capabilities => self.plane.refresh_agent_capabilities()?,
                Kind::TelegramPoll | Kind::TelegramDeliver => {
                    let poll = matches!(self.kind, Kind::TelegramPoll);
                    if poll {
                        self.plane.telegram().resume_updates(cursor.as_deref())?;
                    }
                    return Ok(done(
                        None,
                        u64::from(
                            self.plane
                                .telegram()
                                .maintenance_step(poll, budget.remaining())?,
                        ),
                        0,
                    ));
                }
                Kind::Callbacks => {
                    return Ok(done(
                        None,
                        u64::from(self.plane.deliver_hub_callbacks()?),
                        0,
                    ));
                }
            }
            Ok(JobResult::complete())
        };
        match run() {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(job=self.name(),code=%error.code,"maintenance step will retry");
                JobResult {
                    status: JobStatus::Retry {
                        reason: "step_failed".into(),
                    },
                    rows: 0,
                    bytes: 0,
                }
            }
        }
    }
}

pub fn periodic(plane: &ControlPlane) -> Vec<PeriodicJob> {
    use Kind::*;
    [
        (Metrics, 15, 10),
        (StorageHealth, 300, 10),
        (Partitions, 3600, 10),
        (Retention, 60, 1),
        (Logs, 300, 1),
        (Tests, 30, 10),
        (Previews, 60, 10),
        (Routes, 60, 10),
        (Reviews, 5, 10),
        (Capabilities, 60, 10),
        (Callbacks, 1, 10),
        (TelegramPoll, 1, 4),
        (TelegramDeliver, 1, 4),
    ]
    .into_iter()
    .map(|(kind, interval, seconds)| PeriodicJob {
        run_on_start: matches!(kind, Routes),
        job: Arc::new(ServiceJob {
            plane: plane.clone(),
            kind,
        }),
        interval: Duration::from_secs(interval),
        budget: Duration::from_secs(seconds),
        max_rows: 256,
        max_bytes: 256 * 1024,
    })
    .collect()
}
