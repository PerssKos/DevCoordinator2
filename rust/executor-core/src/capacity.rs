use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::ExecutorError;

const BROKER_SCHEMA: u8 = 1;
const MAX_BROKER_MESSAGE_BYTES: usize = 8192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermitRequest {
    pub run_id: String,
    pub leaf_id: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapacityObservation {
    pub learned_capacity: Option<u32>,
    pub effective_capacity: Option<u32>,
    pub waited: bool,
}

pub trait CapacityPermit: Send {}

pub struct AcquiredPermit {
    pub observation: CapacityObservation,
    pub(crate) _guard: Box<dyn CapacityPermit>,
}

pub type PermitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AcquiredPermit, ExecutorError>> + Send + 'a>>;

pub trait PermitProvider: Send + Sync {
    fn acquire<'a>(&'a self, request: PermitRequest) -> PermitFuture<'a>;
    fn reserve<'a>(&'a self, _run: &'a str, _check: &'a str) -> ReservationFuture<'a> {
        Box::pin(async {
            Ok(ResourceReservation {
                _guard: Box::new(NoReservation),
            })
        })
    }
}

pub struct ResourceReservation {
    _guard: Box<dyn CapacityPermit + Sync>,
}
struct NoReservation;
impl CapacityPermit for NoReservation {}
pub type ReservationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResourceReservation, ExecutorError>> + Send + 'a>>;

struct CheckPermitProvider {
    inner: Arc<dyn PermitProvider>,
    run: String,
    check: String,
    reservation: OnceCell<Arc<ResourceReservation>>,
}

struct ProcessReservation {
    _process: AcquiredPermit,
    _reservation: Arc<ResourceReservation>,
}
impl CapacityPermit for ProcessReservation {}

struct SharedReservation {
    _reservation: Arc<ResourceReservation>,
}
impl CapacityPermit for SharedReservation {}

impl PermitProvider for CheckPermitProvider {
    fn reserve<'a>(&'a self, _: &'a str, _: &'a str) -> ReservationFuture<'a> {
        Box::pin(async move {
            let reservation = self
                .reservation
                .get_or_try_init(|| async {
                    self.inner
                        .reserve(&self.run, &self.check)
                        .await
                        .map(Arc::new)
                })
                .await?
                .clone();
            Ok(ResourceReservation {
                _guard: Box::new(SharedReservation {
                    _reservation: reservation,
                }),
            })
        })
    }
    fn acquire<'a>(&'a self, request: PermitRequest) -> PermitFuture<'a> {
        Box::pin(async move {
            let reservation = self
                .reservation
                .get_or_try_init(|| async {
                    self.inner
                        .reserve(&self.run, &self.check)
                        .await
                        .map(Arc::new)
                })
                .await?
                .clone();
            let process = self.inner.acquire(request).await?;
            Ok(AcquiredPermit {
                observation: process.observation,
                _guard: Box::new(ProcessReservation {
                    _process: process,
                    _reservation: reservation,
                }),
            })
        })
    }
}

pub(crate) fn for_check(
    inner: Arc<dyn PermitProvider>,
    run: &str,
    check: &devcoordinator2_executor_protocol::CheckPlan,
) -> Arc<dyn PermitProvider> {
    if check.resources.is_empty() {
        return inner;
    }
    Arc::new(CheckPermitProvider {
        inner,
        run: run.into(),
        check: check.name.clone(),
        reservation: OnceCell::new(),
    })
}

#[derive(Clone)]
pub struct LocalPermitProvider {
    semaphore: Option<Arc<Semaphore>>,
    limit: Option<u32>,
}

impl LocalPermitProvider {
    pub fn unbounded() -> Self {
        Self {
            semaphore: None,
            limit: None,
        }
    }

    pub fn new(limit: usize) -> Result<Self, ExecutorError> {
        if limit == 0 {
            return Err(ExecutorError::new("local capacity must be positive"));
        }
        Ok(Self {
            semaphore: Some(Arc::new(Semaphore::new(limit))),
            limit: Some(u32::try_from(limit).unwrap_or(u32::MAX)),
        })
    }
}

struct LocalPermit {
    _permit: Option<OwnedSemaphorePermit>,
}
impl CapacityPermit for LocalPermit {}

impl PermitProvider for LocalPermitProvider {
    fn acquire<'a>(&'a self, _request: PermitRequest) -> PermitFuture<'a> {
        Box::pin(async move {
            let (permit, waited) =
                match &self.semaphore {
                    Some(semaphore) => match semaphore.clone().try_acquire_owned() {
                        Ok(permit) => (Some(permit), false),
                        Err(TryAcquireError::NoPermits) => (
                            Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                                ExecutorError::new("local capacity provider closed")
                            })?),
                            true,
                        ),
                        Err(TryAcquireError::Closed) => {
                            return Err(ExecutorError::new("local capacity provider closed"));
                        }
                    },
                    None => (None, false),
                };
            Ok(AcquiredPermit {
                observation: CapacityObservation {
                    learned_capacity: self.limit,
                    effective_capacity: self.limit,
                    waited,
                },
                _guard: Box::new(LocalPermit { _permit: permit }),
            })
        })
    }
}

#[derive(Clone, Debug)]
pub struct UnixPermitProvider {
    socket: PathBuf,
}

impl UnixPermitProvider {
    pub fn new(socket: impl Into<PathBuf>) -> Result<Self, ExecutorError> {
        let socket = socket.into();
        if !socket.is_absolute() {
            return Err(ExecutorError::new(
                "capacity broker socket must be an absolute path",
            ));
        }
        Ok(Self { socket })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct AcquireRequest<'a> {
    schema: u8,
    action: &'static str,
    run_id: &'a str,
    leaf_id: &'a str,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseRequest<'a> {
    schema: u8,
    action: &'static str,
    run_id: &'a str,
    leaf_id: &'a str,
    permit_id: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerResponse {
    schema: u8,
    status: String,
    permit_id: Option<String>,
    learned_capacity: Option<u32>,
    effective_capacity: Option<u32>,
    waited: Option<bool>,
    error: Option<String>,
}

struct UnixPermit {
    stream: Option<UnixStream>,
    run_id: String,
    leaf_id: String,
    permit_id: String,
}

impl CapacityPermit for UnixPermit {}

impl Drop for UnixPermit {
    fn drop(&mut self) {
        let Some(stream) = self.stream.take() else {
            return;
        };
        let release = ReleaseRequest {
            schema: BROKER_SCHEMA,
            action: "release",
            run_id: &self.run_id,
            leaf_id: &self.leaf_id,
            permit_id: &self.permit_id,
        };
        if let Ok(mut payload) = serde_json::to_vec(&release) {
            payload.push(b'\n');
            let _ = stream.try_write(&payload);
        }
        // Closing this connection is the authoritative crash-safe release.
    }
}

impl PermitProvider for UnixPermitProvider {
    fn reserve<'a>(&'a self, run: &'a str, check: &'a str) -> ReservationFuture<'a> {
        Box::pin(async move {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|_| ExecutorError::new("resource broker is unavailable"))?;
            let request = AcquireRequest {
                schema: BROKER_SCHEMA,
                action: "reserve",
                run_id: run,
                leaf_id: check,
            };
            let mut payload =
                serde_json::to_vec(&request).map_err(|e| ExecutorError::new(e.to_string()))?;
            payload.push(b'\n');
            stream
                .write_all(&payload)
                .await
                .map_err(|_| ExecutorError::new("cannot submit resource reservation"))?;
            let response: BrokerResponse =
                serde_json::from_slice(&read_line_bounded(&mut stream).await?)
                    .map_err(|_| ExecutorError::new("invalid resource reservation response"))?;
            if response.schema != BROKER_SCHEMA || response.status != "reserved" {
                return Err(ExecutorError::new("resource reservation was denied"));
            }
            struct SocketReservation {
                _stream: UnixStream,
            }
            impl CapacityPermit for SocketReservation {}
            Ok(ResourceReservation {
                _guard: Box::new(SocketReservation { _stream: stream }),
            })
        })
    }
    fn acquire<'a>(&'a self, request: PermitRequest) -> PermitFuture<'a> {
        Box::pin(async move {
            let mut stream = UnixStream::connect(&self.socket).await.map_err(|error| {
                ExecutorError::new(format!("cannot connect to capacity broker: {error}"))
            })?;
            let acquire = AcquireRequest {
                schema: BROKER_SCHEMA,
                action: "acquire",
                run_id: &request.run_id,
                leaf_id: &request.leaf_id,
            };
            let mut payload = serde_json::to_vec(&acquire).map_err(|error| {
                ExecutorError::new(format!("cannot encode capacity request: {error}"))
            })?;
            payload.push(b'\n');
            stream.write_all(&payload).await.map_err(|error| {
                ExecutorError::new(format!("cannot send capacity request: {error}"))
            })?;
            let response_payload = read_line_bounded(&mut stream).await?;
            let response: BrokerResponse =
                serde_json::from_slice(&response_payload).map_err(|error| {
                    ExecutorError::new(format!("invalid capacity broker response: {error}"))
                })?;
            if response.schema != BROKER_SCHEMA {
                return Err(ExecutorError::new("capacity broker schema mismatch"));
            }
            if response.status != "granted" {
                return Err(ExecutorError::new(
                    response
                        .error
                        .unwrap_or_else(|| "capacity broker denied the leaf".into()),
                ));
            }
            let permit_id = response
                .permit_id
                .ok_or_else(|| ExecutorError::new("capacity grant omitted permit_id"))?;
            let learned = response
                .learned_capacity
                .ok_or_else(|| ExecutorError::new("capacity grant omitted learned_capacity"))?;
            let effective = response
                .effective_capacity
                .ok_or_else(|| ExecutorError::new("capacity grant omitted effective_capacity"))?;
            let waited = response
                .waited
                .ok_or_else(|| ExecutorError::new("capacity grant omitted waited"))?;
            if learned == 0 || effective == 0 || effective > learned {
                return Err(ExecutorError::new(
                    "capacity broker returned invalid capacity values",
                ));
            }
            Ok(AcquiredPermit {
                observation: CapacityObservation {
                    learned_capacity: Some(learned),
                    effective_capacity: Some(effective),
                    waited,
                },
                _guard: Box::new(UnixPermit {
                    stream: Some(stream),
                    run_id: request.run_id,
                    leaf_id: request.leaf_id,
                    permit_id,
                }),
            })
        })
    }
}

async fn read_line_bounded(stream: &mut UnixStream) -> Result<Vec<u8>, ExecutorError> {
    let mut result = Vec::new();
    let mut block = [0_u8; 512];
    loop {
        let read = stream.read(&mut block).await.map_err(|error| {
            ExecutorError::new(format!("cannot read capacity response: {error}"))
        })?;
        if read == 0 {
            return Err(ExecutorError::new(
                "capacity broker closed without a response",
            ));
        }
        if let Some(newline) = block[..read].iter().position(|byte| *byte == b'\n') {
            if newline + 1 != read {
                return Err(ExecutorError::new(
                    "capacity broker response contains trailing data",
                ));
            }
            result.extend_from_slice(&block[..newline]);
            if result.len() > MAX_BROKER_MESSAGE_BYTES {
                return Err(ExecutorError::new("capacity broker response exceeds 8 KiB"));
            }
            return Ok(result);
        }
        result.extend_from_slice(&block[..read]);
        if result.len() > MAX_BROKER_MESSAGE_BYTES {
            return Err(ExecutorError::new("capacity broker response exceeds 8 KiB"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::UnixListener;
    use tokio::sync::Barrier;

    use super::*;

    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn local_provider_releases_permit_on_drop() {
        let provider = Arc::new(LocalPermitProvider::new(1).expect("provider"));
        let first = provider
            .acquire(PermitRequest {
                run_id: "run".into(),
                leaf_id: "one".into(),
            })
            .await
            .expect("first permit");
        assert_eq!(first.observation.learned_capacity, Some(1));
        assert_eq!(first.observation.effective_capacity, Some(1));
        assert!(!first.observation.waited);
        let barrier = Arc::new(Barrier::new(2));
        let task_provider = provider.clone();
        let task_barrier = barrier.clone();
        let waiter = tokio::spawn(async move {
            task_barrier.wait().await;
            task_provider
                .acquire(PermitRequest {
                    run_id: "run".into(),
                    leaf_id: "two".into(),
                })
                .await
        });
        barrier.wait().await;
        assert!(!waiter.is_finished());
        drop(first);
        let second = waiter.await.expect("join").expect("second permit");
        assert!(second.observation.waited);
    }

    #[tokio::test]
    async fn unix_provider_holds_connection_and_emits_release() {
        let sequence = SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "dc2-capacity-test-{}-{sequence}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let acquire = read_line_bounded(&mut stream).await.expect("acquire");
            let request: serde_json::Value =
                serde_json::from_slice(&acquire).expect("request JSON");
            assert_eq!(request["action"], "acquire");
            assert_eq!(request["leaf_id"], "check/case/one");
            stream
                .write_all(
                    b"{\"schema\":1,\"status\":\"granted\",\"permit_id\":\"p1\",\"learned_capacity\":64,\"effective_capacity\":48,\"waited\":true,\"error\":null}\n",
                )
                .await
                .expect("grant");
            let release = read_line_bounded(&mut stream).await.expect("release");
            let release: serde_json::Value =
                serde_json::from_slice(&release).expect("release JSON");
            assert_eq!(release["action"], "release");
            assert_eq!(release["permit_id"], "p1");
        });
        let provider = UnixPermitProvider::new(&socket).expect("provider");
        let permit = provider
            .acquire(PermitRequest {
                run_id: "run".into(),
                leaf_id: "check/case/one".into(),
            })
            .await
            .expect("permit");
        assert_eq!(permit.observation.learned_capacity, Some(64));
        assert_eq!(permit.observation.effective_capacity, Some(48));
        assert!(permit.observation.waited);
        drop(permit);
        server.await.expect("server");
        fs::remove_file(socket).expect("remove socket");
    }

    #[tokio::test]
    async fn unix_provider_rejects_oversized_response_with_final_newline() {
        let sequence = SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "dc2-capacity-oversize-{}-{sequence}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_line_bounded(&mut stream).await.expect("acquire");
            let mut oversized = vec![b'x'; MAX_BROKER_MESSAGE_BYTES + 1];
            oversized.push(b'\n');
            stream.write_all(&oversized).await.expect("oversized reply");
        });
        let provider = UnixPermitProvider::new(&socket).expect("provider");
        let error = provider
            .acquire(PermitRequest {
                run_id: "run".into(),
                leaf_id: "leaf".into(),
            })
            .await
            .err()
            .expect("oversized broker response must fail");
        assert!(error.to_string().contains("exceeds 8 KiB"));
        server.await.expect("server");
        fs::remove_file(socket).expect("remove socket");
    }
}
