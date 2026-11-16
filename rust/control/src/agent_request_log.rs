//! Privacy-safe lifecycle logging for agent-facing control requests.
//!
//! This is intentionally a log projection rather than a second execution
//! ledger. It records every request that reaches the daemon socket, including
//! malformed, rejected, cancelled, and transport-failed requests. Raw
//! parameters, protocol request IDs, paths, identities, and error details are
//! never emitted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use devcoordinator2_api::{ClientKind, ErrorCode, RequestEnvelope, ResponseEnvelope};
use serde_json::Value;

static NEXT_REQUEST_REF: AtomicU64 = AtomicU64::new(1);
static NEXT_MCP_REF: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct McpToolObservation {
    mcp_ref: u64,
    started: Instant,
    tool: String,
    phase: &'static str,
    status: &'static str,
}

impl McpToolObservation {
    pub(crate) fn new(tool: &str) -> Self {
        let mcp_ref = NEXT_MCP_REF.fetch_add(1, Ordering::Relaxed);
        let tool = safe_operation(tool);
        tracing::info!(
            target: "devcoordinator2.agent_tool",
            mcp_ref,
            tool = %tool,
            interface = "mcp",
            phase = "validate",
            "agent tool validation started"
        );
        Self {
            mcp_ref,
            started: Instant::now(),
            tool,
            phase: "validate",
            // The MCP adapter's early returns are validation failures. A
            // successful discovery or dispatch path explicitly changes this
            // to `returned`; a cancellation is also a rejected call at this
            // boundary and the daemon request logger records its transport
            // outcome separately when it reached the socket.
            status: "rejected",
        }
    }

    pub(crate) fn record_returned(&mut self) {
        self.phase = "return";
        self.status = "returned";
    }
}

impl Drop for McpToolObservation {
    fn drop(&mut self) {
        tracing::info!(
            target: "devcoordinator2.agent_tool",
            mcp_ref = self.mcp_ref,
            tool = %self.tool,
            interface = "mcp",
            phase = self.phase,
            status = self.status,
            duration_ms = self.started.elapsed().as_millis(),
            "agent tool validation finished"
        );
    }
}

#[derive(Debug)]
pub(crate) struct RequestObservation {
    request_ref: u64,
    started: Instant,
    operation: Option<String>,
    client: &'static str,
    model: Option<String>,
    effort: Option<String>,
    request_bytes: usize,
    response_bytes: usize,
    work_context: bool,
    work_operation: bool,
    phase: &'static str,
    status: &'static str,
    error_code: Option<String>,
}

impl RequestObservation {
    pub(crate) fn new() -> Self {
        let request_ref = NEXT_REQUEST_REF.fetch_add(1, Ordering::Relaxed);
        let observation = Self {
            request_ref,
            started: Instant::now(),
            operation: None,
            client: "unknown",
            model: None,
            effort: None,
            request_bytes: 0,
            response_bytes: 0,
            work_context: false,
            work_operation: false,
            phase: "read",
            status: "incomplete",
            error_code: None,
        };
        tracing::info!(
            target: "devcoordinator2.agent_tool",
            request_ref,
            phase = "read",
            "agent tool request started"
        );
        observation
    }

    pub(crate) fn record_raw_request(&mut self, raw: &[u8]) {
        self.request_bytes = raw.len();
        let Some(operation) = serde_json::from_slice::<Value>(raw).ok().and_then(|value| {
            value
                .get("operation")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }) else {
            return;
        };
        self.operation = Some(safe_operation(&operation));
    }

    pub(crate) fn record_request(&mut self, request: &RequestEnvelope) {
        self.operation = Some(safe_operation(&request.operation));
        self.client = client_name(request.client.kind);
        self.model = request.client.model.as_deref().map(safe_value);
        self.effort = request.client.effort.as_deref().map(safe_value);
        self.work_context = request.client.work.is_some();
        self.work_operation = request
            .client
            .work
            .as_ref()
            .and_then(|work| work.operation_id.as_ref())
            .is_some();
        self.phase = "dispatch";
    }

    pub(crate) fn record_response(&mut self, response: &ResponseEnvelope, bytes: usize) {
        self.response_bytes = bytes;
        match response {
            ResponseEnvelope::Success { .. } => {
                self.status = "succeeded";
                self.error_code = None;
            }
            ResponseEnvelope::Failure { error, .. } => {
                self.status = "rejected";
                self.error_code = Some(error.code.to_string());
            }
        }
        self.phase = "write";
    }

    pub(crate) fn record_protocol_failure(&mut self, phase: &'static str, code: ErrorCode) {
        self.phase = phase;
        self.status = "rejected";
        self.error_code = Some(code.to_string());
    }

    pub(crate) fn record_transport_failure(&mut self, phase: &'static str) {
        self.phase = phase;
        self.status = "transport_failed";
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        let duration_ms = self.started.elapsed().as_millis();
        tracing::info!(
            target: "devcoordinator2.agent_tool",
            request_ref = self.request_ref,
            operation = self.operation.as_deref().unwrap_or("<unparsed>"),
            client = self.client,
            model = self.model.as_deref().unwrap_or("<unknown>"),
            effort = self.effort.as_deref().unwrap_or("<unknown>"),
            request_bytes = self.request_bytes,
            response_bytes = self.response_bytes,
            work_context = self.work_context,
            work_operation = self.work_operation,
            phase = self.phase,
            status = self.status,
            error_code = self.error_code.as_deref().unwrap_or(""),
            duration_ms,
            "agent tool request finished"
        );
    }
}

fn safe_operation(value: &str) -> String {
    if value.len() <= 128 && !value.chars().any(char::is_control) {
        value.to_owned()
    } else {
        "<invalid>".to_owned()
    }
}

fn safe_value(value: &str) -> String {
    if value.len() <= 160 && !value.chars().any(char::is_control) {
        value.to_owned()
    } else {
        "<invalid>".to_owned()
    }
}

fn client_name(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::Codex => "codex",
        ClientKind::Claude => "claude",
        ClientKind::Cursor => "cursor",
        ClientKind::Antigravity => "antigravity",
        ClientKind::Human => "human",
        ClientKind::Edge => "edge",
        ClientKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::{ClientContext, ResponseEnvelope};
    use serde_json::json;

    #[test]
    fn raw_request_logging_keeps_only_a_safe_operation_name() {
        let mut observation = RequestObservation::new();
        observation.record_raw_request(
            b"{\"operation\":\"plan.overview\",\"params\":{\"path\":\"/private\"}}\n",
        );
        assert_eq!(observation.operation.as_deref(), Some("plan.overview"));

        observation.record_raw_request(b"{\"operation\":\"bad\\u0000name\"}\n");
        assert_eq!(observation.operation.as_deref(), Some("<invalid>"));
    }

    #[test]
    fn request_context_is_reduced_to_bounded_metadata() {
        let mut observation = RequestObservation::new();
        let request = RequestEnvelope {
            protocol: 3,
            id: "opaque-request-id".into(),
            operation: "agent.instruction.current".into(),
            params: json!({"secret":"must-not-be-logged"}),
            client: ClientContext {
                kind: ClientKind::Codex,
                model: Some("gpt-6.1-sol".into()),
                effort: Some("high".into()),
                work: Some(devcoordinator2_api::work_context::WorkContext {
                    version: 1,
                    alarm: None,
                    native_project_id: "project".into(),
                    thread_id: "thread".into(),
                    turn_id: None,
                    operation_id: Some("operation".into()),
                    workstream_id: None,
                    outcome_id: None,
                    experiment_ref: None,
                }),
                ..ClientContext::default()
            },
        };
        observation.record_request(&request);
        assert_eq!(
            observation.operation.as_deref(),
            Some("agent.instruction.current")
        );
        assert_eq!(observation.client, "codex");
        assert!(observation.work_context);
        assert!(observation.work_operation);
        assert!(!format!("{observation:?}").contains("secret"));
    }

    #[test]
    fn response_failures_are_classified_without_copying_details() {
        let mut observation = RequestObservation::new();
        let response = ResponseEnvelope::failure(
            "opaque-request-id",
            devcoordinator2_api::ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "private detail must not be logged",
            ),
        );
        observation.record_response(&response, 128);
        assert_eq!(observation.status, "rejected");
        assert_eq!(observation.error_code.as_deref(), Some("params_invalid"));
        assert_eq!(observation.response_bytes, 128);
    }
}
