//! Deployment state observation through the existing owned-event scheduler.
use std::sync::Arc;

use super::Deployments;
use crate::access::Caller;
use crate::daemon::{DeferredOperation, WaitContext};
use crate::events::{EventService, EventVisibility};
use crate::request_deadline::{observation_deadline, parse_deadline, wait_deadline_error};
use devcoordinator2_api::{ErrorCode, ProtocolError, params, results};

type Visibility = Arc<dyn Fn() -> Result<EventVisibility, ProtocolError> + Send + Sync>;

impl Deployments {
    pub(crate) fn prepare_wait(
        &self,
        request: params::DeploymentWait,
        caller: Caller,
        events: EventService,
        visibility: Visibility,
        wait: WaitContext,
    ) -> Result<DeferredOperation, ProtocolError> {
        let current = self.status(
            request.path.as_deref(),
            request.name.as_deref(),
            request.deployment_id.as_deref(),
            &caller,
        )?;
        if deployment_wait_satisfied(&current, request.state) {
            return Ok(Box::pin(async move { encode(current) }));
        }
        let service = self.clone();
        let deployment_id = current.deployment_id;
        let deadline = parse_deadline(&request.deadline_at)?;
        Ok(Box::pin(async move {
            loop {
                if tokio::time::Instant::now() >= deadline {
                    return Err(wait_deadline_error(
                        "deployment did not reach the requested state before the deadline",
                        None,
                        None,
                        Some(deployment_id.clone()),
                    ));
                }
                let subscription_events = events.clone();
                let subscription_visibility = visibility.clone();
                let target = deployment_id.clone();
                let heartbeat = observation_deadline(&request.deadline_at)?;
                let subscription = wait
                    .blocking(move || {
                        subscription_events.subscribe_with_visibility(
                            params::EventWait {
                                cursor: None,
                                filters: vec![params::EventFilter {
                                    filter_id: "deployment-wait".into(),
                                    categories: vec![params::EventCategory::Deployment],
                                    kinds: vec![],
                                    repository_ids: vec![],
                                    deployment_ids: vec![target],
                                    deadline_at: Some(heartbeat),
                                }],
                                limit: 10,
                            },
                            subscription_visibility,
                        )
                    })
                    .await?;
                // Arm first: a change between the first status read and this
                // subscription must not require an event that is already past.
                let status_service = service.clone();
                let status_request = request.clone();
                let status_caller = caller.clone();
                let latest = wait
                    .blocking(move || {
                        status_service.status(
                            status_request.path.as_deref(),
                            status_request.name.as_deref(),
                            status_request.deployment_id.as_deref(),
                            &status_caller,
                        )
                    })
                    .await?;
                if deployment_wait_satisfied(&latest, request.state) {
                    return encode(latest);
                }
                // Heartbeats also observe out-of-band source changes. They do
                // not extend the absolute preparation/observation budget.
                let _ = subscription.receive().await?;
            }
        }))
    }
}

fn encode(status: results::DeploymentStatus) -> Result<serde_json::Value, ProtocolError> {
    serde_json::to_value(results::DeploymentWaitResult {
        completed: true,
        timed_out: false,
        status,
    })
    .map_err(|_| ProtocolError::new(ErrorCode::InternalError, "cannot encode deployment wait"))
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
