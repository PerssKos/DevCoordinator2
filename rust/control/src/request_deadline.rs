//! One absolute budget for a passive wait's preparation and observation.
use std::future::Future;
use std::sync::Arc;

use devcoordinator2_api::{ErrorCode, ProtocolError};
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct WaitDeadline {
    at: tokio::time::Instant,
    run_id: Option<String>,
    deployment_id: Option<String>,
}

/// Admission for synchronous steps of a passive observer. Idle waits hold no
/// capacity; a started worker keeps its permit after its caller disappears.
#[derive(Clone)]
pub struct WaitContext {
    gate: Arc<tokio::sync::Semaphore>,
    deadline: Option<WaitDeadline>,
}

impl WaitContext {
    pub(crate) fn new(gate: Arc<tokio::sync::Semaphore>, deadline: Option<WaitDeadline>) -> Self {
        Self { gate, deadline }
    }

    pub async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, ProtocolError> + Send + 'static,
    ) -> Result<T, ProtocolError> {
        let admitted = async {
            let permit = self.gate.clone().acquire_owned().await.map_err(|_| {
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "passive observation lane is unavailable",
                )
            })?;
            blocking_operation(move || {
                let _permit = permit;
                work()
            })
            .await
        };
        if let Some(deadline) = &self.deadline {
            deadline.run(admitted).await
        } else {
            admitted.await
        }
    }
}

pub(crate) async fn blocking_operation<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ProtocolError> + Send + 'static,
) -> Result<T, ProtocolError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "operation task failed")
            .with_detail(error.to_string())
    })?
}

impl WaitDeadline {
    pub(crate) fn for_operation(
        operation: &str,
        params: &Value,
    ) -> Result<Option<Self>, ProtocolError> {
        if !matches!(
            operation,
            "test.wait" | "test.admission.wait" | "deployment.wait"
        ) {
            // Event waits own several independently due filter deadlines.
            return Ok(None);
        }
        let value = params
            .get("deadline_at")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::ParamsInvalid, "deadline_at is required")
            })?;
        Ok(Some(Self {
            at: parse_deadline(value)?,
            run_id: params
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            deployment_id: params
                .get("deployment_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }))
    }

    pub(crate) async fn run<T>(
        &self,
        work: impl Future<Output = Result<T, ProtocolError>>,
    ) -> Result<T, ProtocolError> {
        tokio::time::timeout_at(self.at, work)
            .await
            .unwrap_or_else(|_| {
                Err(wait_deadline_error(
                    "the wait did not complete before its requested deadline",
                    None,
                    self.run_id.clone(),
                    self.deployment_id.clone(),
                ))
            })
    }
}

pub(crate) fn parse_deadline(value: &str) -> Result<tokio::time::Instant, ProtocolError> {
    let parsed = time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "deadline_at must be an RFC 3339 timestamp",
            )
        })?;
    let duration = parsed - time::OffsetDateTime::now_utc();
    Ok(tokio::time::Instant::now() + duration.try_into().unwrap_or_default())
}

/// Recheck externally observable state through the existing event scheduler,
/// without extending the caller's absolute deadline or creating another timer.
pub(crate) fn observation_deadline(value: &str) -> Result<String, ProtocolError> {
    let deadline =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "deadline_at must be an RFC 3339 timestamp",
                )
            })?;
    deadline
        .min(time::OffsetDateTime::now_utc() + time::Duration::seconds(1))
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "cannot format observation deadline",
            )
        })
}

pub(crate) fn wait_deadline_error(
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
