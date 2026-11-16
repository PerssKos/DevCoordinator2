use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use devcoordinator2_api::{
    EmptyParams, ErrorCode, MAX_REQUEST_BYTES, PingData, ProtocolError, RequestEnvelope,
    ResponseEnvelope, encode_response, parse_request,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{error, info};

use crate::access::Caller;
use crate::agent_request_log::RequestObservation;
pub use crate::request_deadline::WaitContext;
use crate::request_deadline::{WaitDeadline, blocking_operation};
use crate::{DATABASE_SCHEMA_VERSION, SOURCE_COMMIT};

const READ_DEADLINE: Duration = Duration::from_secs(5);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);

/// Synchronous domain boundary executed on Tokio's blocking pool. Domain
/// services own their own serialization and may invoke bounded local process
/// adapters without blocking the async socket reactor.
pub trait OperationExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        operation: &str,
        params: Value,
        caller: &Caller,
    ) -> Result<Value, ProtocolError>;

    /// Select optional blocking preparation without doing target I/O here.
    /// The returned hold lives until execution finishes, including after a
    /// finite request's caller disconnects.
    fn prepare_admission(
        &self,
        _operation: &str,
        _params: Value,
        _caller: &Caller,
    ) -> Option<AdmissionPreparation> {
        None
    }

    /// Prepare a passive observer. Every later synchronous subscription or
    /// status step must use the supplied context's blocking admission.
    fn defer(
        &self,
        _operation: &str,
        _params: Value,
        _caller: &Caller,
        _wait: WaitContext,
    ) -> Option<Result<DeferredOperation, ProtocolError>> {
        None
    }
}

pub type DeferredOperation =
    Pin<Box<dyn Future<Output = Result<Value, ProtocolError>> + Send + 'static>>;

pub type AdmissionHold = Box<dyn Send>;
pub type AdmissionWait =
    Pin<Box<dyn Future<Output = Result<AdmissionHold, ProtocolError>> + Send + 'static>>;
pub type AdmissionPreparation =
    Box<dyn FnOnce() -> Result<Option<PreparedAdmission>, ProtocolError> + Send + 'static>;

pub enum PreparedAdmission {
    Ready(AdmissionHold),
    Waiting(AdmissionWait),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone)]
struct PingExecutor {
    socket_display: Arc<str>,
}

impl OperationExecutor for PingExecutor {
    fn execute(
        &self,
        operation: &str,
        params: Value,
        _caller: &Caller,
    ) -> Result<Value, ProtocolError> {
        if operation != "ping" {
            return Err(ProtocolError::new(
                ErrorCode::InternalError,
                "operation handler is not installed",
            ));
        }
        serde_json::from_value::<EmptyParams>(params).map_err(|error| {
            ProtocolError::new(ErrorCode::ParamsInvalid, "ping parameters are invalid")
                .with_detail(error.to_string())
        })?;
        serde_json::to_value(PingData {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: devcoordinator2_api::PROTOCOL_VERSION,
            schema_version: DATABASE_SCHEMA_VERSION,
            executor_schema: devcoordinator2_executor_protocol::EXECUTOR_SCHEMA,
            source_commit: SOURCE_COMMIT.to_owned(),
            socket: self.socket_display.to_string(),
        })
        .map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot encode ping result")
                .with_detail(error.to_string())
        })
    }
}

#[derive(Clone)]
pub struct App {
    edge_uid: Option<u32>,
    executor: Arc<dyn OperationExecutor>,
    installation_endpoint: Option<std::path::PathBuf>,
    request_slots: Arc<Semaphore>,
    read_slots: Arc<Semaphore>,
    action_slots: Arc<Semaphore>,
    wait_preparations: Arc<Semaphore>,
    admission_handoffs: Arc<Semaphore>,
}

impl App {
    pub fn new(socket_path: &Path) -> Self {
        let socket_display: Arc<str> = socket_path.display().to_string().into();
        Self {
            edge_uid: None,
            executor: Arc::new(PingExecutor { socket_display }),
            installation_endpoint: None,
            request_slots: Arc::new(Semaphore::new(16)),
            read_slots: Arc::new(Semaphore::new(16)),
            action_slots: Arc::new(Semaphore::new(4)),
            wait_preparations: Arc::new(Semaphore::new(8)),
            admission_handoffs: Arc::new(Semaphore::new(4)),
        }
    }

    pub fn with_executor(edge_uid: Option<u32>, executor: Arc<dyn OperationExecutor>) -> Self {
        Self {
            edge_uid,
            executor,
            installation_endpoint: None,
            request_slots: Arc::new(Semaphore::new(16)),
            read_slots: Arc::new(Semaphore::new(16)),
            action_slots: Arc::new(Semaphore::new(4)),
            wait_preparations: Arc::new(Semaphore::new(8)),
            admission_handoffs: Arc::new(Semaphore::new(4)),
        }
    }

    pub fn with_installation_fence(mut self, socket_path: std::path::PathBuf) -> Self {
        self.installation_endpoint = Some(socket_path);
        self
    }

    pub(crate) async fn wait_for_installation_fence(&self) -> std::io::Result<()> {
        let Some(endpoint) = &self.installation_endpoint else {
            return std::future::pending().await;
        };
        let parent = endpoint.parent().unwrap_or_else(|| Path::new("."));
        let mut events = crate::socket_endpoint::SocketEvents::new(parent)?;
        loop {
            if endpoint.with_file_name("daemon.pre-cutover.sock").exists() {
                return Ok(());
            }
            events.changed().await?;
        }
    }

    pub async fn dispatch(
        &self,
        request: RequestEnvelope,
        peer: PeerCredentials,
    ) -> ResponseEnvelope {
        let id = request.id.clone();
        if self.installation_endpoint.as_ref().is_some_and(|path| {
            // Candidate reads support frozen-source comparisons. A path could
            // implicitly register a repository, so only exact-identity reads
            // are allowed before committing normal mutation admission.
            path.with_file_name("daemon.pre-cutover.sock").exists()
                && (!path.exists()
                    || !devcoordinator2_api::operation(&request.operation)
                        .is_some_and(|op| op.policy.read_only() && !op.passive_wait())
                    || request
                        .params
                        .get("path")
                        .is_some_and(|value| !value.is_null()))
        }) {
            return ResponseEnvelope::failure(
                id,
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "installation in progress; re-query after recovery",
                ),
            );
        }
        if crate::client::is_passive_wait_operation(&request.operation) {
            match self.deferred(&request, peer).await {
                Ok(Some(deferred)) => return response_from_result(id, deferred.await),
                Ok(None) => {}
                Err(error) => return ResponseEnvelope::failure(id, error),
            }
        }
        let caller = match Caller::from_client(
            peer.pid,
            peer.uid,
            peer.gid,
            request.client.clone(),
            self.edge_uid,
        ) {
            Ok(caller) => caller,
            Err(error) => return ResponseEnvelope::failure(id, error),
        };
        let long_running =
            devcoordinator2_api::operation(&request.operation).is_some_and(|op| op.long_running());
        let admission = if long_running {
            match self.admission(&request, &caller).await {
                Ok(hold) => hold,
                Err(error) => return ResponseEnvelope::failure(id, error),
            }
        } else {
            None
        };
        if admission.is_some()
            && self
                .installation_endpoint
                .as_ref()
                .is_some_and(|path| path.with_file_name("daemon.pre-cutover.sock").exists())
        {
            return ResponseEnvelope::failure(
                id,
                ProtocolError::new(
                    ErrorCode::DaemonUnavailable,
                    "installation began during admission; this mutation was not started",
                ),
            );
        }
        let operation = request.operation;
        let slots = if long_running {
            &self.action_slots
        } else if devcoordinator2_api::operation(&operation)
            .is_some_and(|definition| definition.policy.read_only())
        {
            &self.read_slots
        } else {
            &self.request_slots
        };
        let permit = match Arc::clone(slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return ResponseEnvelope::failure(id, overloaded()),
        };
        let params = request.params;
        let executor = Arc::clone(&self.executor);
        response_from_result(
            id,
            blocking_operation(move || {
                let _permit = permit;
                let _admission = admission;
                executor.execute(&operation, params, &caller)
            })
            .await,
        )
    }

    async fn admission(
        &self,
        request: &RequestEnvelope,
        caller: &Caller,
    ) -> Result<Option<AdmissionHold>, ProtocolError> {
        let Some(prepare) =
            self.executor
                .prepare_admission(&request.operation, request.params.clone(), caller)
        else {
            return Ok(None);
        };
        let preparation = Arc::clone(&self.wait_preparations)
            .try_acquire_owned()
            .map_err(|_| overloaded())?;
        // Preparation may resolve an authorized target, but its blocking task
        // and every database connection finish before the returned wait runs.
        let wait = blocking_operation(move || {
            let _preparation = preparation;
            prepare()
        })
        .await?;
        match wait {
            Some(PreparedAdmission::Ready(hold)) => Ok(Some(hold)),
            Some(PreparedAdmission::Waiting(wait)) => {
                let _handoff = Arc::clone(&self.admission_handoffs)
                    .try_acquire_owned()
                    .map_err(|_| overloaded())?;
                wait.await.map(Some)
            }
            None => Ok(None),
        }
    }

    async fn deferred(
        &self,
        request: &RequestEnvelope,
        peer: PeerCredentials,
    ) -> Result<Option<DeferredOperation>, ProtocolError> {
        if !crate::client::is_passive_wait_operation(&request.operation) {
            return Ok(None);
        }
        if self
            .installation_endpoint
            .as_ref()
            .is_some_and(|path| path.with_file_name("daemon.pre-cutover.sock").exists())
        {
            return Err(ProtocolError::new(
                ErrorCode::DaemonUnavailable,
                "installation in progress; reconnect this wait after recovery",
            ));
        }
        let permit = Arc::clone(&self.wait_preparations)
            .try_acquire_owned()
            .map_err(|_| overloaded())?;
        let caller = Caller::from_client(
            peer.pid,
            peer.uid,
            peer.gid,
            request.client.clone(),
            self.edge_uid,
        )?;
        let executor = Arc::clone(&self.executor);
        let operation = request.operation.clone();
        let params = request.params.clone();
        let deadline = WaitDeadline::for_operation(&operation, &params)?;
        let context = WaitContext::new(self.wait_preparations.clone(), deadline.clone());
        let preparation = blocking_operation(move || {
            // The worker owns this permit through real completion. Dropping
            // a timed-out observer cannot admit more blocked preparations.
            let _permit = permit;
            executor
                .defer(&operation, params, &caller, context)
                .transpose()
        });
        let prepared = if let Some(deadline) = &deadline {
            deadline.run(preparation).await?
        } else {
            preparation.await?
        };
        Ok(prepared.map(|operation| {
            if let Some(deadline) = deadline {
                Box::pin(async move { deadline.run(operation).await }) as DeferredOperation
            } else {
                operation
            }
        }))
    }
}

fn overloaded() -> ProtocolError {
    use devcoordinator2_api::recovery::{Action, Class, Guidance, RecoveryOption};
    ProtocolError::new(
        ErrorCode::Busy,
        "Coordinator request lane is full; this request was not started",
    )
    .with_recovery(Guidance {
        class: Class::Transient,
        retryable: true,
        waitable: false,
        safe_to_continue: true,
        state: Some("not_admitted".into()),
        reason: "Bounded request admission rejected this request before execution".into(),
        run_id: None,
        deployment_id: None,
        generation: None,
        repository_id: None,
        options: vec![RecoveryOption {
            id: "retry-not-admitted".into(),
            action: Action::Retry,
            operation: None,
            effect: "Retry this unstarted request after pending work completes".into(),
            target: None,
            cost: Some("bounded backoff".into()),
            risk: Some("none; request was not started".into()),
            prerequisites: vec![],
            independent_work_safe: true,
            user_action_required: false,
        }],
        field_errors: vec![],
        example: None,
    })
}

fn response_from_result(id: String, result: Result<Value, ProtocolError>) -> ResponseEnvelope {
    match result {
        Ok(data) => ResponseEnvelope::success(id.clone(), data)
            .unwrap_or_else(|error| ResponseEnvelope::failure(id, error)),
        Err(error) => ResponseEnvelope::failure(id, error),
    }
}

pub async fn serve(socket_path: &Path, mut shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
    let app = Arc::new(App::new(socket_path));
    serve_with_app(socket_path, &mut shutdown, app).await
}

pub struct DaemonEndpoint {
    endpoint: crate::socket_endpoint::SocketEndpoint,
    changes: crate::socket_endpoint::SocketEvents,
    path: std::path::PathBuf,
}

impl DaemonEndpoint {
    pub async fn bind(socket_path: &Path) -> std::io::Result<Self> {
        let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let changes = crate::socket_endpoint::SocketEvents::new(parent)?;
        let endpoint = crate::socket_endpoint::SocketEndpoint::bind(socket_path).await?;
        set_socket_mode(socket_path)?;
        Ok(Self {
            endpoint,
            changes,
            path: socket_path.to_owned(),
        })
    }

    pub async fn serve(
        self,
        shutdown: &mut watch::Receiver<bool>,
        app: Arc<App>,
    ) -> std::io::Result<()> {
        let Self {
            mut endpoint,
            mut changes,
            path,
        } = self;
        info!(socket = %path.display(), "serving protocol 3");
        let mut connections = JoinSet::new();
        let (observer_interrupt, _) = watch::channel(false);
        while !*shutdown.borrow() {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                changed = changes.changed() => {
                    changed?;
                    match endpoint.recover_missing() {
                        Ok(true) => {
                            set_socket_mode(&path)?;
                            info!("restored missing control endpoint without restarting application work");
                        }
                        Ok(false) => {}
                        Err(error) => error!(%error, "control endpoint recovery refused"),
                    }
                    observer_interrupt.send_replace(endpoint.is_fenced());
                }
                accepted = endpoint.listener.accept() => {
                    let (stream, _) = accepted?;
                    if connections.len() >= 256 { drop(stream); continue; }
                    let app = Arc::clone(&app);
                    let interruptions = observer_interrupt.subscribe();
                    connections.spawn(async move {
                        if let Err(error) = serve_connection(stream, app, interruptions).await {
                            error!(%error, "connection failed");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        error!(%error, "connection task failed");
                    }
                }
            }
        }
        observer_interrupt.send_replace(true);
        drop(endpoint);
        while let Some(completed) = connections.join_next().await {
            if let Err(error) = completed {
                error!(%error, "connection task failed during shutdown");
            }
        }
        Ok(())
    }
}

pub async fn serve_with_app(
    socket_path: &Path,
    shutdown: &mut watch::Receiver<bool>,
    app: Arc<App>,
) -> std::io::Result<()> {
    DaemonEndpoint::bind(socket_path)
        .await?
        .serve(shutdown, app)
        .await
}

async fn serve_connection(
    mut stream: UnixStream,
    app: Arc<App>,
    mut interruptions: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut observation = RequestObservation::new();
    let credentials = match stream.peer_cred() {
        Ok(credentials) => credentials,
        Err(error) => {
            observation.record_transport_failure("peer_credentials");
            return Err(error);
        }
    };
    let peer = PeerCredentials {
        pid: credentials
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0),
        uid: credentials.uid(),
        gid: credentials.gid(),
    };
    let raw = match timeout(READ_DEADLINE, read_frame(&mut stream)).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(error)) => {
            observation.record_transport_failure("read");
            return Err(error);
        }
        Err(_) => {
            let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "request read timed out");
            observation.record_transport_failure("read");
            return Err(error);
        }
    };
    observation.record_raw_request(&raw);
    let id = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let response = if !raw.ends_with(b"\n") {
        observation.record_protocol_failure("parse", ErrorCode::ProtocolInvalid);
        ResponseEnvelope::failure(
            id,
            ProtocolError::new(
                ErrorCode::ProtocolInvalid,
                "request must be newline terminated",
            ),
        )
    } else {
        match parse_request(&raw) {
            Ok(request) => {
                observation.record_request(&request);
                let id = request.id.clone();
                let observer = crate::client::is_passive_wait_operation(&request.operation);
                match app.deferred(&request, peer).await {
                    Ok(Some(deferred)) => {
                        let mut unexpected = [0_u8; 1];
                        let result = tokio::select! {
                            _ = interruptions.wait_for(|interrupted| *interrupted), if observer => {
                                Some(ResponseEnvelope::failure(
                                    id.clone(),
                                    crate::client::reconnectable_wait_error(
                                        &request,
                                        "coordinator is restarting; reconnect the same wait using its original identity",
                                    ),
                                ))
                            }
                            result = deferred => Some(response_from_result(id.clone(), result)),
                            read = stream.read(&mut unexpected) => match read {
                                Ok(0) | Err(_) => None,
                                Ok(_) => Some(ResponseEnvelope::failure(
                                    id.clone(),
                                    ProtocolError::new(
                                        ErrorCode::ProtocolInvalid,
                                        "wait connection sent data after its request frame",
                                    ),
                                )),
                            },
                        };
                        let Some(response) = result else {
                            observation.record_transport_failure("wait");
                            return Ok(());
                        };
                        response
                    }
                    Ok(None) => app.dispatch(request, peer).await,
                    Err(error) => ResponseEnvelope::failure(id, error),
                }
            }
            Err(error) => {
                observation.record_protocol_failure("parse", error.code);
                ResponseEnvelope::failure(id, error)
            }
        }
    };
    let encoded = encode_response(&response);
    observation.record_response(&response, encoded.len());
    match timeout(WRITE_DEADLINE, stream.write_all(&encoded)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            observation.record_transport_failure("write");
            return Err(error);
        }
        Err(_) => {
            let error =
                std::io::Error::new(std::io::ErrorKind::TimedOut, "response write timed out");
            observation.record_transport_failure("write");
            return Err(error);
        }
    }
    match timeout(WRITE_DEADLINE, stream.shutdown()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            observation.record_transport_failure("shutdown");
            Err(error)
        }
        Err(_) => {
            let error =
                std::io::Error::new(std::io::ErrorKind::TimedOut, "response shutdown timed out");
            observation.record_transport_failure("shutdown");
            Err(error)
        }
    }
}

async fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 8192];
    while raw.len() <= MAX_REQUEST_BYTES {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.ends_with(b"\n") {
            break;
        }
    }
    Ok(raw)
}

#[cfg(unix)]
fn set_socket_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::{ClientContext, PROTOCOL_VERSION, ResponseEnvelope};
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct WaitingExecutor {
        started: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        dropped: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    const WAIT_OPERATIONS: [&str; 4] = [
        "event.wait",
        "test.admission.wait",
        "test.wait",
        "deployment.wait",
    ];

    fn wait_params(operation: &str) -> Value {
        let deadline = "2100-01-01T00:00:00Z";
        match operation {
            "event.wait" => {
                serde_json::json!({"filters":[{"filter_id":"fixture","categories":["health"],"deadline_at":deadline}]})
            }
            "test.admission.wait" => serde_json::json!({"deadline_at":deadline}),
            "test.wait" => {
                serde_json::json!({"path":"/fixture","run_id":"fixture-run","deadline_at":deadline})
            }
            "deployment.wait" => {
                serde_json::json!({"deployment_id":"fixture-deployment","state":"ready","deadline_at":deadline})
            }
            _ => panic!("unexpected wait fixture"),
        }
    }

    fn wait_frame(operation: &str) -> Vec<u8> {
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "protocol":PROTOCOL_VERSION,"id":"wait","operation":operation,"params":wait_params(operation),"client":{}
        })).unwrap();
        frame.push(b'\n');
        frame
    }

    struct EventExecutor {
        events: crate::events::EventService,
    }

    struct CompletingWaitExecutor {
        started: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    }

    impl OperationExecutor for EventExecutor {
        fn execute(
            &self,
            _operation: &str,
            _params: Value,
            _caller: &Caller,
        ) -> Result<Value, ProtocolError> {
            unreachable!("event fixture only serves deferred waits")
        }

        fn defer(
            &self,
            operation: &str,
            params: Value,
            _caller: &Caller,
            _wait: WaitContext,
        ) -> Option<Result<DeferredOperation, ProtocolError>> {
            if operation != "event.wait" {
                return None;
            }
            Some((|| {
                let request = serde_json::from_value(params).map_err(|error| {
                    ProtocolError::new(ErrorCode::ParamsInvalid, "invalid event wait")
                        .with_detail(error.to_string())
                })?;
                let subscription = self
                    .events
                    .subscribe(request, crate::events::EventVisibility::unrestricted())?;
                Ok(Box::pin(async move {
                    serde_json::to_value(subscription.receive().await?).map_err(|error| {
                        ProtocolError::new(ErrorCode::InternalError, "cannot encode event wait")
                            .with_detail(error.to_string())
                    })
                }) as DeferredOperation)
            })())
        }
    }

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    impl OperationExecutor for WaitingExecutor {
        fn execute(
            &self,
            _operation: &str,
            _params: Value,
            _caller: &Caller,
        ) -> Result<Value, ProtocolError> {
            unreachable!("wait operations are deferred")
        }

        fn defer(
            &self,
            operation: &str,
            _params: Value,
            _caller: &Caller,
            _wait: WaitContext,
        ) -> Option<Result<DeferredOperation, ProtocolError>> {
            if !crate::client::is_passive_wait_operation(operation) {
                return None;
            }
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            // The prepared future owns cleanup before its first poll.
            let signal = DropSignal(self.dropped.lock().unwrap().take());
            let release = self.release.clone();
            Some(Ok(Box::pin(async move {
                let _signal = signal;
                match release {
                    Some(release) => {
                        release.notified().await;
                        Ok(serde_json::json!({"completed":true}))
                    }
                    None => std::future::pending::<Result<Value, ProtocolError>>().await,
                }
            })))
        }
    }

    impl OperationExecutor for CompletingWaitExecutor {
        fn execute(
            &self,
            _operation: &str,
            _params: Value,
            _caller: &Caller,
        ) -> Result<Value, ProtocolError> {
            unreachable!("wait operations are deferred")
        }

        fn defer(
            &self,
            operation: &str,
            _params: Value,
            _caller: &Caller,
            _wait: WaitContext,
        ) -> Option<Result<DeferredOperation, ProtocolError>> {
            if !crate::client::is_passive_wait_operation(operation) {
                return None;
            }
            let started = self.started.lock().unwrap().take();
            let release = self.release.lock().unwrap().take();
            Some(Ok(Box::pin(async move {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                if let Some(release) = release {
                    let _ = release.await;
                }
                Ok(serde_json::json!({"completed": true}))
            })))
        }
    }

    #[tokio::test]
    async fn duplicate_daemon_start_preserves_the_active_endpoint() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve(&server_socket, shutdown_rx).await });
        timeout(Duration::from_secs(2), async {
            while !socket.exists() {
                assert!(!server.is_finished());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first endpoint readiness");
        let (_duplicate_tx, duplicate_rx) = watch::channel(false);
        let duplicate = timeout(Duration::from_secs(1), serve(&socket, duplicate_rx)).await;
        let response = crate::client::call(
            &socket,
            "ping",
            serde_json::json!({}),
            ClientContext::default(),
        )
        .await;
        shutdown_tx.send(true).expect("shutdown");
        server.await.expect("server task").expect("server shutdown");
        let error = duplicate
            .expect("duplicate must be rejected promptly")
            .expect_err("duplicate is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(matches!(response, Ok(ResponseEnvelope::Success { .. })));
    }

    #[tokio::test]
    async fn ping_crosses_real_unix_socket() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve(&server_socket, shutdown_rx).await });
        while !socket.exists() {
            assert!(
                !server.is_finished(),
                "server failed before binding its socket"
            );
            tokio::task::yield_now().await;
        }
        let response = crate::client::call(
            &socket,
            "ping",
            serde_json::json!({}),
            ClientContext::default(),
        )
        .await
        .expect("ping response");
        assert!(matches!(response, ResponseEnvelope::Success { .. }));
        let spoofed = crate::client::call(
            &socket,
            "ping",
            serde_json::json!({}),
            ClientContext {
                identity: Some("spoof@example.test".to_owned()),
                ..ClientContext::default()
            },
        )
        .await
        .expect("bounded denial");
        assert!(matches!(
            spoofed,
            ResponseEnvelope::Failure { error, .. }
                if error.code == ErrorCode::PermissionDenied
        ));
        shutdown_tx.send(true).expect("shutdown");
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn endpoint_loss_recovers_without_cancelling_an_accepted_operation() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(WaitingExecutor {
                started: Arc::new(Mutex::new(Some(started_tx))),
                dropped: Arc::new(Mutex::new(Some(dropped_tx))),
                release: None,
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let server_socket = socket.clone();
        let server =
            tokio::spawn(
                async move { serve_with_app(&server_socket, &mut shutdown_rx, app).await },
            );
        timeout(Duration::from_secs(2), async {
            while !socket.exists() {
                assert!(!server.is_finished());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("socket readiness");
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"{\"protocol\":3,\"id\":\"wait\",\"operation\":\"event.wait\",\"params\":{\"filters\":[{\"filter_id\":\"retained\",\"categories\":[\"health\"]}]},\"client\":{}}\n").await.unwrap();
        timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let mut changes = crate::socket_endpoint::SocketEvents::new(temporary.path()).unwrap();
        std::fs::remove_file(&socket).expect("remove only the isolated fixture socket");
        timeout(Duration::from_secs(2), async {
            while !socket.exists() {
                changes.changed().await.expect("directory change");
            }
        })
        .await
        .expect("endpoint recovery");
        let replacement_connection = UnixStream::connect(&socket)
            .await
            .expect("recovered listener");
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o666
        );
        assert!(matches!(
            dropped_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(!server.is_finished());
        drop(replacement_connection);
        stream.shutdown().await.unwrap();
        drop(stream);
        timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        shutdown_tx.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn installation_fence_accepts_the_replacement_endpoint() {
        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("custom.sock");
        let fence = temporary.path().join("daemon.pre-cutover.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let app = App::new(&socket).with_installation_fence(socket.clone());
        let ping = || {
            serde_json::from_value(serde_json::json!({
                "protocol":3,"id":"fence-proof","operation":"ping","params":{},"client":{}
            }))
            .unwrap()
        };
        let peer = PeerCredentials {
            pid: 1,
            uid: 1000,
            gid: 1000,
        };
        assert!(matches!(
            app.dispatch(ping(), peer).await,
            ResponseEnvelope::Success { .. }
        ));
        std::fs::rename(&socket, &fence).unwrap();
        assert!(
            matches!(app.dispatch(ping(), peer).await, ResponseEnvelope::Failure {error,..} if error.code == ErrorCode::DaemonUnavailable)
        );
        drop(listener);
        let _replacement = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let replacement = App::new(&socket).with_installation_fence(socket.clone());
        assert!(fence.exists());
        assert!(matches!(
            replacement.dispatch(ping(), peer).await,
            ResponseEnvelope::Success { .. }
        ));
        let mut mutation = ping();
        mutation.operation = "repository.register".into();
        assert!(
            matches!(replacement.dispatch(mutation, peer).await, ResponseEnvelope::Failure {error,..} if error.code == ErrorCode::DaemonUnavailable)
        );
        std::fs::remove_file(&fence).unwrap();
        std::fs::rename(&socket, &fence).unwrap();
        assert!(
            matches!(replacement.dispatch(ping(), peer).await, ResponseEnvelope::Failure {error,..} if error.code == ErrorCode::DaemonUnavailable)
        );
    }

    #[tokio::test]
    async fn installer_fence_releases_idle_observers() {
        for operation in WAIT_OPERATIONS {
            verify_observer_interruption(false, operation).await;
        }
    }

    #[tokio::test]
    async fn shutdown_releases_idle_observers() {
        for operation in WAIT_OPERATIONS {
            verify_observer_interruption(true, operation).await;
        }
    }

    async fn verify_observer_interruption(shutdown: bool, operation: &str) {
        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(WaitingExecutor {
                started: Arc::new(Mutex::new(Some(started_tx))),
                dropped: Arc::new(Mutex::new(Some(dropped_tx))),
                release: None,
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let server = tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, app).await });
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream.write_all(&wait_frame(operation)).await.unwrap();
        timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        if shutdown {
            shutdown_tx.send(true).unwrap();
        } else {
            std::fs::rename(&socket, temporary.path().join("daemon.pre-cutover.sock")).unwrap();
        }
        let result = timeout(Duration::from_secs(2), read_frame(&mut stream)).await;
        if result.is_err() {
            drop(stream);
            let _ = shutdown_tx.send(true);
            let _ = timeout(Duration::from_secs(2), server).await;
            panic!("idle observer prevented the upgrade boundary");
        }
        let response: ResponseEnvelope = serde_json::from_slice(&result.unwrap().unwrap()).unwrap();
        assert!(
            matches!(response, ResponseEnvelope::Failure { error, .. } if error.code == ErrorCode::DaemonUnavailable)
        );
        timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        if !shutdown {
            assert!(!socket.exists());
            std::fs::rename(temporary.path().join("daemon.pre-cutover.sock"), &socket).unwrap();
            assert!(UnixStream::connect(&socket).await.is_ok());
            shutdown_tx.send(true).unwrap();
        }
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_preserves_finite_accepted_requests() {
        struct FiniteExecutor {
            started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl OperationExecutor for FiniteExecutor {
            fn execute(&self, _: &str, _: Value, _: &Caller) -> Result<Value, ProtocolError> {
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(serde_json::json!({"completed": true}))
            }
        }
        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(FiniteExecutor {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let server = tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, app).await });
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"{\"protocol\":3,\"id\":\"finite\",\"operation\":\"ping\",\"params\":{},\"client\":{}}\n").await.unwrap();
        timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        shutdown_tx.send(true).unwrap();
        assert!(!server.is_finished());
        release_tx.send(()).unwrap();
        let response: Value = serde_json::from_slice(
            &timeout(Duration::from_secs(2), read_frame(&mut stream))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["data"]["completed"], true);
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn disconnect_cancels_a_deferred_event_wait() {
        for operation in WAIT_OPERATIONS {
            let temporary = tempdir().expect("tempdir");
            let socket = temporary.path().join("daemon.sock");
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
            let app = Arc::new(App::with_executor(
                None,
                Arc::new(WaitingExecutor {
                    started: Arc::new(Mutex::new(Some(started_tx))),
                    dropped: Arc::new(Mutex::new(Some(dropped_tx))),
                    release: None,
                }),
            ));
            let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
            let server_socket = socket.clone();
            let server =
                tokio::spawn(
                    async move { serve_with_app(&server_socket, &mut shutdown_rx, app).await },
                );
            while !socket.exists() {
                tokio::task::yield_now().await;
            }
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            stream.write_all(&wait_frame(operation)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .expect("deferred operation start deadline")
                .expect("start signal");
            stream.shutdown().await.unwrap();
            drop(stream);
            tokio::time::timeout(Duration::from_secs(2), dropped_rx)
                .await
                .expect("deferred operation drop deadline")
                .expect("drop signal");
            shutdown_tx.send(true).unwrap();
            server.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn passive_test_wait_keeps_the_socket_open_until_the_result() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(CompletingWaitExecutor {
                started: Arc::new(Mutex::new(Some(started_tx))),
                release: Arc::new(Mutex::new(Some(release_rx))),
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let server_socket = socket.clone();
        let server =
            tokio::spawn(
                async move { serve_with_app(&server_socket, &mut shutdown_rx, app).await },
            );
        while !socket.exists() {
            assert!(!server.is_finished());
            tokio::task::yield_now().await;
        }
        let waiting_socket = socket.clone();
        let waiting = tokio::spawn(async move {
            crate::client::call(
                &waiting_socket,
                "test.wait",
                serde_json::json!({
                    "path":"/worktree",
                    "run_id":"accepted-run",
                    "deadline_at":"2099-01-01T00:00:00Z"
                }),
                ClientContext::default(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        release_tx.send(()).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .expect("wait response");
        assert!(matches!(response, ResponseEnvelope::Success { .. }));
        shutdown_tx.send(true).unwrap();
        server.await.unwrap().unwrap();
    }

    // Exercise both real transports on one reactor thread. A separate thread
    // releases slow synchronous preparation only after an unrelated request
    // executes, or at the failure ceiling so a regression cannot hang the suite.
    #[tokio::test(flavor = "current_thread")]
    async fn slow_wait_preparation_keeps_both_transports_responsive() {
        struct IntentCount(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for IntentCount {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        struct SlowPreparation {
            started: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            ping: std::sync::mpsc::Sender<()>,
            fail: bool,
            admission: bool,
            admission_release: watch::Receiver<bool>,
            intents: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl OperationExecutor for SlowPreparation {
            fn execute(
                &self,
                operation: &str,
                _: Value,
                _: &Caller,
            ) -> Result<Value, ProtocolError> {
                if operation == "ping" {
                    let _ = self.ping.send(());
                } else {
                    assert!(matches!(operation, "deployment.restart" | "plan.recovery"));
                }
                Ok(serde_json::json!({"responsive": true}))
            }

            fn prepare_admission(
                &self,
                operation: &str,
                params: Value,
                _: &Caller,
            ) -> Option<AdmissionPreparation> {
                if !self.admission || operation != "deployment.restart" {
                    return None;
                }
                let intents = self.intents.clone();
                let mut release = self.admission_release.clone();
                let started = self.started.clone();
                let fail = self.fail;
                Some(Box::new(move || {
                    if params["deployment_id"] == "independent" {
                        return Ok(Some(PreparedAdmission::Ready(Box::new(()))));
                    }
                    intents.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let intent = IntentCount(intents);
                    let started = started.lock().unwrap().take();
                    Ok(Some(PreparedAdmission::Waiting(Box::pin(async move {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        release.wait_for(|released| *released).await.unwrap();
                        if fail {
                            Err(ProtocolError::new(
                                ErrorCode::Busy,
                                "handoff was not admitted",
                            ))
                        } else {
                            Ok(Box::new(intent) as AdmissionHold)
                        }
                    }))))
                }))
            }

            fn defer(
                &self,
                operation: &str,
                _: Value,
                _: &Caller,
                _: WaitContext,
            ) -> Option<Result<DeferredOperation, ProtocolError>> {
                if operation != "test.wait" {
                    return None;
                }
                if self.admission {
                    return Some(Ok(Box::pin(async {
                        Ok(serde_json::json!({"completed": true}))
                    })));
                }
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                Some(if self.fail {
                    Err(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "slow preparation failed",
                    ))
                } else {
                    Ok(Box::pin(async {
                        Ok(serde_json::json!({"completed": true}))
                    }))
                })
            }
        }

        let mut outcomes = Vec::new();
        for bridge in [false, true] {
            for fail in [false, true] {
                for admission in [false, true] {
                    let temporary = tempdir().unwrap();
                    let socket = temporary.path().join("daemon.sock");
                    let directory = temporary.path().join("bridge");
                    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel();
                    let (admission_release_tx, admission_release_rx) = watch::channel(false);
                    let intents = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let (ping_tx, ping_rx) = std::sync::mpsc::channel();
                    let watchdog = std::thread::spawn(move || {
                        let responsive = ping_rx.recv_timeout(Duration::from_secs(2)).is_ok();
                        release_tx.send(()).unwrap();
                        admission_release_tx.send_replace(true);
                        responsive
                    });
                    let app = Arc::new(App::with_executor(
                        None,
                        Arc::new(SlowPreparation {
                            started: Arc::new(Mutex::new(Some(started_tx))),
                            release: Mutex::new(release_rx),
                            ping: ping_tx,
                            fail,
                            admission,
                            admission_release: admission_release_rx,
                            intents: intents.clone(),
                        }),
                    ));
                    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
                    let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
                    let socket_app = app.clone();
                    let server =
                        tokio::spawn(
                            async move { endpoint.serve(&mut shutdown_rx, socket_app).await },
                        );
                    let bridge_directory = directory.clone();
                    let bridge_shutdown = shutdown_tx.subscribe();
                    let bridge_app = app.clone();
                    let bridge_server = tokio::spawn(async move {
                        crate::sandbox_bridge::serve(bridge_app, &bridge_directory, bridge_shutdown)
                            .await
                    });
                    while !directory.join("daemon.lock").exists() {
                        assert!(!bridge_server.is_finished());
                        tokio::task::yield_now().await;
                    }

                    let request_socket = socket.clone();
                    let request_directory = directory.clone();
                    let waiting = tokio::spawn(async move {
                        let (operation, params) = if admission {
                            (
                                "deployment.restart",
                                serde_json::json!({"deployment_id":"held"}),
                            )
                        } else {
                            (
                                "test.wait",
                                serde_json::json!({"path":"/fixture", "run_id":"accepted-run", "deadline_at":"2099-01-01T00:00:00Z"}),
                            )
                        };
                        if bridge {
                            let request = serde_json::json!({"protocol":3, "id":"aabbcc", "operation":operation, "params":params, "client":{}});
                            std::fs::write(
                                request_directory.join("aabbcc.request"),
                                serde_json::to_vec(&request).unwrap(),
                            )
                            .unwrap();
                            timeout(Duration::from_secs(4), async {
                                loop {
                                    if let Ok(raw) =
                                        std::fs::read(request_directory.join("aabbcc.response"))
                                    {
                                        break serde_json::from_slice::<ResponseEnvelope>(&raw)
                                            .unwrap();
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .unwrap()
                        } else {
                            crate::client::call(
                                &request_socket,
                                operation,
                                params,
                                ClientContext::default(),
                            )
                            .await
                            .unwrap()
                        }
                    });
                    timeout(Duration::from_secs(4), started_rx)
                        .await
                        .unwrap()
                        .unwrap();
                    let mut extra = Vec::new();
                    if admission {
                        let cancellation_app = app.clone();
                        let cancellation_request = serde_json::from_value(serde_json::json!({
                        "protocol":3, "id":"cancelled-admission", "operation":"deployment.restart",
                        "params":{"deployment_id":"held"}, "client":{}
                    })).unwrap();
                        let cancelled = tokio::spawn(async move {
                            cancellation_app
                                .dispatch(
                                    cancellation_request,
                                    PeerCredentials {
                                        pid: 0,
                                        uid: 1000,
                                        gid: 1000,
                                    },
                                )
                                .await
                        });
                        timeout(Duration::from_secs(1), async {
                            while app.admission_handoffs.available_permits() != 2 {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        cancelled.abort();
                        assert!(cancelled.await.unwrap_err().is_cancelled());
                        assert_eq!(app.admission_handoffs.available_permits(), 3);
                        assert_eq!(intents.load(std::sync::atomic::Ordering::SeqCst), 1);
                        for _ in 0..3 {
                            let socket = socket.clone();
                            extra.push(tokio::spawn(async move {
                                crate::client::call(
                                    &socket,
                                    "deployment.restart",
                                    serde_json::json!({"deployment_id":"held"}),
                                    ClientContext::default(),
                                )
                                .await
                                .unwrap()
                            }));
                        }
                        timeout(Duration::from_secs(1), async {
                            while app.admission_handoffs.available_permits() != 0
                                || app.wait_preparations.available_permits() != 8
                            {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        assert_eq!(app.action_slots.available_permits(), 4);
                        assert_eq!(app.request_slots.available_permits(), 16);
                        let excess = crate::client::call(
                            &socket,
                            "deployment.restart",
                            serde_json::json!({"deployment_id":"held"}),
                            ClientContext::default(),
                        )
                        .await
                        .unwrap();
                        assert!(
                            matches!(excess, ResponseEnvelope::Failure {error, ..} if error.code == ErrorCode::Busy)
                        );
                        let independent = crate::client::call(
                            &socket,
                            "deployment.restart",
                            serde_json::json!({"deployment_id":"independent"}),
                            ClientContext::default(),
                        )
                        .await
                        .unwrap();
                        assert!(independent.is_ok());
                        let preparations = app
                            .wait_preparations
                            .clone()
                            .try_acquire_many_owned(8)
                            .unwrap();
                        let unrelated = crate::client::call(
                            &socket,
                            "plan.recovery",
                            serde_json::json!({"repository_id":"fixture", "transaction_dir":"/fixture", "backup_sha256":"a".repeat(64)}),
                            ClientContext::default(),
                        )
                        .await
                        .unwrap();
                        assert!(
                            unrelated.is_ok(),
                            "unrelated finite action depended on handoff preparation capacity"
                        );
                        drop(preparations);
                        let passive = crate::client::call(&socket, "test.wait", serde_json::json!({"path":"/fixture", "run_id":"accepted-run", "deadline_at":"2099-01-01T00:00:00Z"}), ClientContext::default()).await.unwrap();
                        assert!(passive.is_ok());
                    }
                    let ping = crate::client::call(
                        &socket,
                        "ping",
                        serde_json::json!({}),
                        ClientContext::default(),
                    )
                    .await
                    .unwrap();
                    let waited = waiting.await.unwrap();
                    for waiting in extra {
                        let response = waiting.await.unwrap();
                        assert_eq!(response.is_ok(), !fail);
                    }
                    assert_eq!(app.admission_handoffs.available_permits(), 4);
                    assert_eq!(intents.load(std::sync::atomic::Ordering::SeqCst), 0);
                    let responsive = watchdog.join().unwrap();
                    let expected_wait = if fail {
                        matches!(waited, ResponseEnvelope::Failure { error, .. } if error.code == if admission { ErrorCode::Busy } else { ErrorCode::ParamsInvalid })
                    } else {
                        matches!(waited, ResponseEnvelope::Success { .. })
                    };
                    outcomes.push((
                        bridge,
                        fail,
                        admission,
                        responsive,
                        ping.is_ok(),
                        expected_wait,
                    ));
                    shutdown_tx.send(true).unwrap();
                    server.await.unwrap().unwrap();
                    bridge_server.await.unwrap().unwrap();
                }
            }
        }
        assert!(
            outcomes
                .iter()
                .all(|(_, _, _, responsive, ping, waited)| *responsive && *ping && *waited),
            "slow preparation blocked unrelated requests: {outcomes:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadlines_and_disconnects_keep_blocked_preparation_capacity_owned() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Condvar, Mutex};

        struct BlockedPreparation {
            active: Arc<AtomicUsize>,
            started: Arc<tokio::sync::Notify>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }
        impl OperationExecutor for BlockedPreparation {
            fn execute(&self, _: &str, _: Value, _: &Caller) -> Result<Value, ProtocolError> {
                Ok(serde_json::json!({"responsive":true}))
            }

            fn defer(
                &self,
                operation: &str,
                _: Value,
                _: &Caller,
                _: WaitContext,
            ) -> Option<Result<DeferredOperation, ProtocolError>> {
                if operation != "test.wait" {
                    return None;
                }
                self.active.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                let (released, changed) = &*self.release;
                let _guard = changed
                    .wait_while(released.lock().unwrap(), |released| !*released)
                    .unwrap();
                self.active.fetch_sub(1, Ordering::SeqCst);
                Some(Ok(Box::pin(async {
                    Ok(serde_json::json!({"complete":true}))
                })))
            }
        }
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (released, changed) = &*self.0;
                *released.lock().unwrap() = true;
                changed.notify_all();
            }
        }

        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_on_exit = Release(release.clone());
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(BlockedPreparation {
                active: active.clone(),
                started: started.clone(),
                release,
            }),
        ));
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let socket_app = app.clone();
        let server =
            tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, socket_app).await });
        let mut waiting = Vec::new();
        for _ in 0..8 {
            let socket = socket.clone();
            waiting.push(tokio::spawn(async move {
                let deadline = (time::OffsetDateTime::now_utc()
                    + time::Duration::milliseconds(300))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
                timeout(
                    Duration::from_secs(1),
                    crate::client::call(
                        &socket,
                        "test.wait",
                        serde_json::json!({
                            "path":"/fixture", "run_id":"held-run", "deadline_at":deadline
                        }),
                        ClientContext::default(),
                    ),
                )
                .await
            }));
        }
        timeout(Duration::from_secs(2), async {
            while active.load(Ordering::SeqCst) != 8 {
                started.notified().await;
            }
        })
        .await
        .unwrap();
        let disconnected = waiting.pop().unwrap();
        disconnected.abort();
        assert!(disconnected.await.unwrap_err().is_cancelled());
        let mut bounded = true;
        for wait in waiting {
            bounded &= matches!(wait.await.unwrap(), Ok(Ok(ResponseEnvelope::Failure { error, .. }))
                if error.code == ErrorCode::WaitDeadlineReached);
        }
        let retained =
            app.wait_preparations.available_permits() == 0 && active.load(Ordering::SeqCst) == 8;
        let mut refused = true;
        for _ in 0..16 {
            refused &= matches!(crate::client::call(
                &socket, "test.wait", serde_json::json!({
                    "path":"/fixture", "run_id":"held-run", "deadline_at":"2099-01-01T00:00:00Z"
                }), ClientContext::default(),
            ).await.unwrap(), ResponseEnvelope::Failure { error, .. } if error.code == ErrorCode::Busy);
        }
        let ping = timeout(
            Duration::from_secs(1),
            crate::client::call(
                &socket,
                "ping",
                serde_json::json!({}),
                ClientContext::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let still_bounded = active.load(Ordering::SeqCst) == 8;
        drop(release_on_exit);
        let restored = timeout(
            Duration::from_secs(2),
            app.wait_preparations.clone().acquire_many_owned(8),
        )
        .await
        .unwrap()
        .unwrap();
        drop(restored);
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(bounded && retained && refused && still_bounded);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(matches!(ping, ResponseEnvelope::Success { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn awakened_observers_keep_worker_capacity_after_deadlines_and_disconnects() {
        use crate::database::{Database, Lane};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counts {
            prepared: AtomicUsize,
            active: AtomicUsize,
            peak: AtomicUsize,
            started: AtomicUsize,
            changed: tokio::sync::Notify,
        }
        struct Active(Arc<Counts>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
        struct Observer {
            database: Database,
            counts: Arc<Counts>,
            wake: watch::Receiver<bool>,
        }
        impl OperationExecutor for Observer {
            fn execute(&self, _: &str, _: Value, _: &Caller) -> Result<Value, ProtocolError> {
                Ok(serde_json::json!({"responsive":true}))
            }
            fn defer(
                &self,
                operation: &str,
                _: Value,
                _: &Caller,
                wait: WaitContext,
            ) -> Option<Result<DeferredOperation, ProtocolError>> {
                if operation != "test.wait" {
                    return None;
                }
                self.counts.prepared.fetch_add(1, Ordering::SeqCst);
                self.counts.changed.notify_one();
                let mut wake = self.wake.clone();
                let counts = self.counts.clone();
                let database = self.database.clone();
                Some(Ok(Box::pin(async move {
                    wake.wait_for(|wake| *wake).await.map_err(|_| {
                        ProtocolError::new(ErrorCode::InternalError, "fixture wake closed")
                    })?;
                    wait.blocking(move || {
                        let active = counts.active.fetch_add(1, Ordering::SeqCst) + 1;
                        counts.peak.fetch_max(active, Ordering::SeqCst);
                        counts.started.fetch_add(1, Ordering::SeqCst);
                        counts.changed.notify_one();
                        let _active = Active(counts);
                        database
                            .read(|connection| {
                                let value = connection
                                    .query_row("SELECT 1", [], |row| row.get::<_, i32>(0))?;
                                Ok(serde_json::json!({"value":value}))
                            })
                            .map_err(|_| {
                                ProtocolError::new(ErrorCode::InternalError, "fixture read failed")
                            })
                    })
                    .await
                })))
            }
        }

        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let database = Database::open(temporary.path().join("authority")).unwrap();
        let held = crate::request_responsiveness_tests::HeldLane::new(database.clone(), Lane::Read);
        let counts = Arc::new(Counts {
            prepared: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let (wake, receive) = watch::channel(false);
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(Observer {
                database,
                counts: counts.clone(),
                wake: receive,
            }),
        ));
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let socket_app = app.clone();
        let server =
            tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, socket_app).await });
        let deadline = (time::OffsetDateTime::now_utc() + time::Duration::seconds(2))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let mut observers = Vec::new();
        for index in 0..32 {
            let socket = socket.clone();
            let deadline = deadline.clone();
            observers.push(tokio::spawn(async move {
                timeout(Duration::from_secs(3), crate::client::call(
                    &socket,"test.wait",serde_json::json!({"path":"/fixture","run_id":"accepted-run","deadline_at":deadline}),ClientContext::default(),
                )).await
            }));
            timeout(Duration::from_secs(1), async {
                while counts.prepared.load(Ordering::SeqCst) <= index {
                    counts.changed.notified().await;
                }
            })
            .await
            .unwrap();
        }
        // All thirty-two waits are idle and admitted, while no synchronous
        // step owns capacity until the event actually wakes them.
        let idle = timeout(
            Duration::from_secs(1),
            app.wait_preparations.clone().acquire_many_owned(8),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(counts.started.load(Ordering::SeqCst), 0);
        drop(idle);
        wake.send_replace(true);
        timeout(Duration::from_secs(1), async {
            while counts.active.load(Ordering::SeqCst) != 8 {
                counts.changed.notified().await;
            }
        })
        .await
        .unwrap();
        for observer in observers.drain(..16) {
            observer.abort();
            assert!(observer.await.unwrap_err().is_cancelled());
        }
        let mut bounded = true;
        for observer in observers {
            bounded &= matches!(observer.await.unwrap(),Ok(Ok(ResponseEnvelope::Failure {error,..})) if error.code==ErrorCode::WaitDeadlineReached);
        }
        let retained = app.wait_preparations.available_permits() == 0
            && counts.active.load(Ordering::SeqCst) == 8;
        let refused = matches!(crate::client::call(
            &socket,"test.wait",serde_json::json!({"path":"/fixture","run_id":"accepted-run","deadline_at":"2099-01-01T00:00:00Z"}),ClientContext::default(),
        ).await.unwrap(),ResponseEnvelope::Failure {error,..} if error.code==ErrorCode::Busy);
        let ping = timeout(
            Duration::from_secs(1),
            crate::client::call(
                &socket,
                "ping",
                serde_json::json!({}),
                ClientContext::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::task::spawn_blocking(move || drop(held))
            .await
            .unwrap();
        let restored = timeout(
            Duration::from_secs(2),
            app.wait_preparations.clone().acquire_many_owned(8),
        )
        .await
        .unwrap()
        .unwrap();
        drop(restored);
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(bounded && retained && refused);
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
        assert_eq!(counts.peak.load(Ordering::SeqCst), 8);
        assert_eq!(
            counts.started.load(Ordering::SeqCst),
            8,
            "cancelled queued observers must not leave worker backlog"
        );
        assert!(matches!(ping, ResponseEnvelope::Success { .. }));
    }

    #[tokio::test]
    async fn passive_wait_interruption_returns_reconnectable_run_identity() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, _dropped_rx) = tokio::sync::oneshot::channel();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(WaitingExecutor {
                started: Arc::new(Mutex::new(Some(started_tx))),
                dropped: Arc::new(Mutex::new(Some(dropped_tx))),
                release: None,
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let server = tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, app).await });
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream
            .write_all(
                br#"{"protocol":3,"id":"wait-run","operation":"test.wait","params":{"path":"/worktree","run_id":"accepted-run","deadline_at":"2099-01-01T00:00:00Z"},"client":{}}
"#,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        shutdown_tx.send(true).unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let ResponseEnvelope::Failure { error, .. } = response else {
            panic!("interrupted wait unexpectedly succeeded");
        };
        assert_eq!(error.code, ErrorCode::DaemonUnavailable);
        let recovery = error.recovery.expect("reconnectable wait guidance");
        assert!(recovery.retryable);
        assert!(recovery.waitable);
        assert_eq!(recovery.run_id.as_deref(), Some("accepted-run"));
        assert_eq!(recovery.options[0].operation.as_deref(), Some("test.wait"));
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn event_wait_crosses_the_real_socket_without_an_event_before_wait_race() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("daemon.sock");
        let database =
            crate::database::Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let events = crate::events::EventService::new(database).unwrap();
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(EventExecutor {
                events: events.clone(),
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let server_socket = socket.clone();
        let server =
            tokio::spawn(
                async move { serve_with_app(&server_socket, &mut shutdown_rx, app).await },
            );
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let wait_socket = socket.clone();
        let waiting = tokio::spawn(async move {
            crate::client::call(
                &wait_socket,
                "event.wait",
                serde_json::json!({
                    "cursor":0,
                    "filters":[{"filter_id":"health","categories":["health"]}]
                }),
                ClientContext::default(),
            )
            .await
        });
        events
            .publish(crate::events::NewEvent {
                occurred_at: "2026-09-04T12:00:00Z".to_owned(),
                event: devcoordinator2_api::results::OwnedEvent::Health(
                    devcoordinator2_api::results::HealthOwnedEvent {
                        kind: "health.changed".to_owned(),
                        repository_id: None,
                        deployment_id: None,
                        subject_kind: "host".to_owned(),
                        subject_id: "host".to_owned(),
                        severity: None,
                    },
                ),
                dedupe_key: Some("socket-health".to_owned()),
            })
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("socket event wait deadline")
            .expect("client task")
            .expect("event response");
        let ResponseEnvelope::Success { data, .. } = response else {
            panic!("event wait failed");
        };
        let result: devcoordinator2_api::results::EventWaitResult =
            serde_json::from_value(data).unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].filter_ids, ["health"]);
        shutdown_tx.send(true).unwrap();
        server.await.unwrap().unwrap();
        for operation in WAIT_OPERATIONS {
            verify_client_wait_completion(operation).await;
        }
    }
    async fn verify_client_wait_completion(operation: &'static str) {
        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("daemon.sock");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let app = Arc::new(App::with_executor(
            None,
            Arc::new(WaitingExecutor {
                started: Arc::new(Mutex::new(Some(started_tx))),
                dropped: Arc::new(Mutex::new(Some(dropped_tx))),
                release: Some(release.clone()),
            }),
        ));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let endpoint = DaemonEndpoint::bind(&socket).await.unwrap();
        let server = tokio::spawn(async move { endpoint.serve(&mut shutdown_rx, app).await });
        let mut client = tokio::spawn(async move {
            crate::client::call(
                &socket,
                operation,
                wait_params(operation),
                ClientContext::default(),
            )
            .await
        });
        timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        if let Ok(result) = timeout(Duration::from_millis(25), &mut client).await {
            shutdown_tx.send(true).unwrap();
            server.await.unwrap().unwrap();
            panic!("{operation} ended before the observed operation completed: {result:?}");
        }
        release.notify_one();
        let response = timeout(Duration::from_secs(2), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            matches!(response, ResponseEnvelope::Success { data, .. } if data["completed"] == true)
        );
        timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        shutdown_tx.send(true).unwrap();
        server.await.unwrap().unwrap();
    }
}
