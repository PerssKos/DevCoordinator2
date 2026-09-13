//! Fair, peer-authenticated host-wide capacity for governed test leaves.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use devcoordinator2_api::results::{Capacity, CapacityAdjustment};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use time::{format_description::FormatItem, macros::format_description};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval, timeout};
use tracing::error;

use crate::database::{Database, DatabaseError};
use crate::platform::{
    Clock, HostClock, HostMonotonicClock, HostRandom, MonotonicClock, RandomSource,
};

pub const CAPACITY_PROTOCOL_SCHEMA: u8 = 1;
pub const CAP_MIN: u32 = 1;
pub const CAP_MAX: u32 = 65_535;
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
pub const MIN_EPOCH_SECONDS: f64 = 600.0;
const PRESSURE_PERCENT: f64 = 98.0;
const RECOVERY_PERCENT: f64 = 95.0;
const UNDERUSED_PERCENT: f64 = 90.0;
pub const MEMORY_EMERGENCY_PERCENT: f64 = 90.0;
const MEMORY_RECOVERY_PERCENT: f64 = 85.0;
const MAX_REQUEST_BYTES: usize = 4096;
const READ_DEADLINE: Duration = Duration::from_secs(5);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

pub trait Metrics: Send + 'static {
    fn sample(&mut self) -> (Option<f64>, Option<f64>);
}

/// Host accounting uses available memory, including reclaimable caches.
#[derive(Clone, Copy, Debug)]
pub struct HostMemory {
    pub total: u64,
    pub available: u64,
}

impl HostMemory {
    pub fn read(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(root.join("meminfo")).ok()?;
        let mut total = None;
        let mut available = None;
        for line in text.lines() {
            let (key, value) = line.split_once(':')?;
            let destination = match key {
                "MemTotal" => &mut total,
                "MemAvailable" => &mut available,
                _ => continue,
            };
            let mut fields = value.split_whitespace();
            let kib = fields.next()?.parse::<u64>().ok()?;
            if fields.next()? != "kB" || destination.is_some() {
                return None;
            }
            *destination = Some(kib.checked_mul(1024)?);
        }
        let value = Self {
            total: total?,
            available: available?,
        };
        value.percent_used().map(|_| value)
    }

    pub fn percent_used(self) -> Option<f64> {
        (self.total > 0 && self.available <= self.total)
            .then(|| 100.0 * (self.total - self.available) as f64 / self.total as f64)
    }

    pub fn emergency(self) -> bool {
        self.percent_used()
            .is_some_and(|used| used >= MEMORY_EMERGENCY_PERCENT)
    }
}

type MemoryPressureHandler = Arc<dyn Fn() + Send + Sync>;

pub struct ProcMetrics {
    root: PathBuf,
    previous_cpu: Option<(u64, u64)>,
}

impl ProcMetrics {
    pub fn new(root: PathBuf) -> Self {
        let previous_cpu = cpu_totals(&root);
        Self { root, previous_cpu }
    }
}

impl Default for ProcMetrics {
    fn default() -> Self {
        Self::new(PathBuf::from("/proc"))
    }
}

impl Metrics for ProcMetrics {
    fn sample(&mut self) -> (Option<f64>, Option<f64>) {
        let current = cpu_totals(&self.root);
        let cpu = current
            .zip(self.previous_cpu)
            .and_then(|(current, previous)| {
                let total = current.0.checked_sub(previous.0)?;
                let idle = current.1.checked_sub(previous.1)?;
                (total > 0 && idle <= total).then_some(100.0 * (total - idle) as f64 / total as f64)
            });
        self.previous_cpu = current;
        (cpu, memory_percent(&self.root))
    }
}

#[derive(Clone)]
pub struct CapacityBroker {
    inner: Arc<Inner>,
}

struct Inner {
    database: Database,
    socket_path: PathBuf,
    state: Mutex<State>,
    notify: Notify,
    clock: Arc<dyn Clock>,
    monotonic: Arc<dyn MonotonicClock>,
    random: Arc<dyn RandomSource>,
    metrics: Mutex<Box<dyn Metrics>>,
    memory_pressure_handler: Mutex<Option<MemoryPressureHandler>>,
    sample_interval: Duration,
    min_epoch_seconds: f64,
}

#[derive(Debug)]
struct State {
    learned: u32,
    cap: Option<u32>,
    registered_runs: BTreeMap<String, u32>,
    queues: BTreeMap<String, VecDeque<u64>>,
    round_robin: VecDeque<String>,
    pending: BTreeMap<u64, Pending>,
    active: BTreeMap<String, ActivePermit>,
    next_pending_id: u64,
    stopping: bool,
    paused: bool,
    memory_emergency: bool,
    cpu_pressure_streak: u8,
    memory_pressure_streak: u8,
    recovery_streak: u8,
    pending_decrease: bool,
    epoch_memory_emergency: bool,
    epoch_started: Option<f64>,
    epoch_runs: BTreeSet<String>,
    samples: Vec<(Option<f64>, Option<f64>, bool)>,
    run_started: BTreeMap<String, f64>,
    run_durations: Vec<f64>,
}

#[derive(Clone, Debug)]
struct Pending {
    run_id: String,
    waited: bool,
    outcome: PendingOutcome,
}

#[derive(Clone, Debug)]
enum PendingOutcome {
    Waiting,
    Granted(Grant),
    Denied(String),
}

#[derive(Clone, Debug)]
struct Grant {
    permit_id: String,
    learned_capacity: u32,
    effective_capacity: u32,
    waited: bool,
}

#[derive(Clone, Debug)]
struct ActivePermit {
    run_id: String,
}

/// Holds exact connection ownership across every I/O error and task cancellation.
struct ConnectionPermit {
    broker: CapacityBroker,
    pending_id: u64,
    permit_id: Option<String>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        if let Some(id) = &self.permit_id {
            let _ = self.broker.release(id);
        }
        let _ = self.broker.cancel_pending(self.pending_id);
    }
}

#[derive(Clone, Debug)]
struct Adjustment {
    actor: String,
    reason: String,
    previous: u32,
    new: u32,
    cpu: Option<f64>,
    memory: Option<f64>,
    saturation: Option<f64>,
    duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireRequest {
    schema: u8,
    action: String,
    run_id: String,
    leaf_id: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct BrokerResponse<'a> {
    schema: u8,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    permit_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    learned_capacity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_capacity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

impl CapacityBroker {
    pub fn new(database: Database, socket_path: PathBuf) -> Result<Self, ProtocolError> {
        let logical_cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1);
        Self::with_adapters(
            database,
            socket_path,
            logical_cpus,
            Arc::new(HostClock),
            Arc::new(HostMonotonicClock::default()),
            Arc::new(HostRandom),
            Box::new(ProcMetrics::default()),
            SAMPLE_INTERVAL,
            MIN_EPOCH_SECONDS,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_adapters(
        database: Database,
        socket_path: PathBuf,
        logical_cpus: usize,
        clock: Arc<dyn Clock>,
        monotonic: Arc<dyn MonotonicClock>,
        random: Arc<dyn RandomSource>,
        metrics: Box<dyn Metrics>,
        sample_interval: Duration,
        min_epoch_seconds: f64,
    ) -> Result<Self, ProtocolError> {
        let initial = u32::try_from(logical_cpus.saturating_mul(2))
            .unwrap_or(CAP_MAX)
            .clamp(CAP_MIN, CAP_MAX);
        let now = timestamp(clock.as_ref())?;
        let (learned, cap) = database
            .transaction(move |transaction| {
                let existing = transaction
                    .query_row(
                        "SELECT learned_capacity,cap FROM test_capacity_state WHERE singleton=1",
                        [],
                        |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Option<u32>>(1)?)),
                    )
                    .optional()?;
                if let Some(existing) = existing {
                    return Ok(existing);
                }
                transaction.execute(
                    "INSERT INTO test_capacity_state(singleton,learned_capacity,cap,updated_at) VALUES(1,?1,NULL,?2)",
                    rusqlite::params![initial, now],
                )?;
                Ok((initial, None))
            })
            .map_err(database_error)?;
        if !(CAP_MIN..=CAP_MAX).contains(&learned)
            || cap.is_some_and(|cap| !(CAP_MIN..=CAP_MAX).contains(&cap))
        {
            return Err(ProtocolError::new(
                ErrorCode::InternalError,
                "stored test capacity is invalid",
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                database,
                socket_path,
                state: Mutex::new(State::new(learned, cap)),
                notify: Notify::new(),
                clock,
                monotonic,
                random,
                metrics: Mutex::new(metrics),
                memory_pressure_handler: Mutex::new(None),
                sample_interval,
                min_epoch_seconds,
            }),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    pub fn register_run(&self, run_id: &str, uid: u32) -> Result<(), ProtocolError> {
        validate_run_id(run_id)?;
        let mut state = self.state()?;
        if let Some(existing) = state.registered_runs.get(run_id)
            && *existing != uid
        {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                format!("run {run_id} is already registered to another uid"),
            ));
        }
        state.registered_runs.insert(run_id.to_owned(), uid);
        Ok(())
    }

    pub fn unregister_run(&self, run_id: &str) -> Result<(), ProtocolError> {
        let now = self.inner.monotonic.seconds();
        let (adjustment, learned, cap) = {
            let mut state = self.state()?;
            state.registered_runs.remove(run_id);
            if let Some(queue) = state.queues.remove(run_id) {
                for pending_id in queue {
                    if let Some(pending) = state.pending.get_mut(&pending_id) {
                        pending.outcome = PendingOutcome::Denied("run is no longer active".into());
                    }
                }
            }
            state.round_robin.retain(|candidate| candidate != run_id);
            state.active.retain(|_, permit| permit.run_id != run_id);
            state.finish_run(run_id, now);
            let adjustment = state.finish_epoch(now, self.inner.min_epoch_seconds);
            state.grant_ready(self.inner.random.as_ref())?;
            (adjustment, state.learned, state.cap)
        };
        if let Some(adjustment) = adjustment {
            self.persist_adjustment(learned, cap, &adjustment)?;
        }
        self.inner.notify.notify_waiters();
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Capacity, ProtocolError> {
        let (learned_capacity, effective_capacity, cap, active, waiting, paused) = {
            let state = self.state()?;
            (
                state.learned,
                state.effective(),
                state.cap,
                saturating_u32(state.active.len()),
                saturating_u32(state.waiting()),
                state.paused,
            )
        };
        let last_adjustment = self
            .inner
            .database
            .call(|connection| {
                connection
                    .query_row(
                        "SELECT event_id,at,actor,reason,previous_capacity,new_capacity,cap,p95_cpu_percent,p95_memory_percent,saturation_fraction,epoch_seconds FROM test_capacity_events ORDER BY event_id DESC LIMIT 1",
                        [],
                        |row| {
                            Ok(CapacityAdjustment {
                                event_id: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                                at: row.get(1)?,
                                actor: row.get(2)?,
                                reason: row.get(3)?,
                                previous_capacity: row.get(4)?,
                                new_capacity: row.get(5)?,
                                cap: row.get(6)?,
                                p95_cpu_percent: row.get(7)?,
                                p95_memory_percent: row.get(8)?,
                                saturation_fraction: row.get(9)?,
                                epoch_seconds: row.get(10)?,
                            })
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        Ok(Capacity {
            learned_capacity,
            effective_capacity,
            cap,
            active,
            waiting,
            paused,
            last_adjustment,
        })
    }

    pub fn set_cap(&self, cap: Option<u32>, actor: &str) -> Result<Capacity, ProtocolError> {
        if cap.is_some_and(|cap| !(CAP_MIN..=CAP_MAX).contains(&cap)) {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("cap must be null or an integer in {CAP_MIN}..{CAP_MAX}"),
            ));
        }
        let changed = {
            let mut state = self.state()?;
            if state.cap == cap {
                false
            } else {
                let prior_cap = state.cap;
                let previous = state.effective();
                state.cap = cap;
                let adjustment = Adjustment {
                    actor: actor.to_owned(),
                    reason: "administrator_cap_changed".into(),
                    previous,
                    new: state.effective(),
                    cpu: None,
                    memory: None,
                    saturation: None,
                    duration: None,
                };
                if let Err(error) = self.persist_adjustment(state.learned, state.cap, &adjustment) {
                    state.cap = prior_cap;
                    return Err(error);
                }
                state.grant_ready(self.inner.random.as_ref())?;
                true
            }
        };
        if changed {
            self.inner.notify.notify_waiters();
        }
        self.snapshot()
    }

    pub fn record_sample(&self, cpu_percent: Option<f64>, memory_percent: Option<f64>) {
        let now = self.inner.monotonic.seconds();
        let adjustment = if let Ok(mut state) = self.inner.state.lock() {
            state.record_sample(
                bounded_percent(cpu_percent),
                bounded_percent(memory_percent),
            );
            let adjustment = state.finish_epoch(now, self.inner.min_epoch_seconds);
            if !state.paused {
                let _ = state.grant_ready(self.inner.random.as_ref());
            }
            adjustment.map(|adjustment| (adjustment, state.learned, state.cap))
        } else {
            None
        };
        if let Some((adjustment, learned, cap)) = adjustment
            && let Err(error) = self.persist_adjustment(learned, cap, &adjustment)
        {
            error!(%error, "capacity sample adjustment could not be persisted");
        }
        self.inner.notify.notify_waiters();
    }

    pub fn set_memory_pressure_handler(&self, handler: MemoryPressureHandler) {
        *self
            .inner
            .memory_pressure_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    pub async fn serve(&self, mut shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
        prepare_socket_path(&self.inner.socket_path).await?;
        let listener = UnixListener::bind(&self.inner.socket_path)?;
        std::fs::set_permissions(
            &self.inner.socket_path,
            std::fs::Permissions::from_mode(0o666),
        )?;
        let mut connections = JoinSet::new();
        let mut emergency = JoinSet::new();
        let mut sampler = interval(self.inner.sample_interval);
        sampler.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break Ok(());
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            let broker = self.clone();
                            connections.spawn(async move {
                                if let Err(error) = broker.serve_connection(stream).await {
                                    error!(%error, "capacity connection failed");
                                }
                            });
                        }
                        Err(error) => break Err(error),
                    }
                }
                _ = sampler.tick() => {
                    let sample = self.inner.metrics.lock().ok().map(|mut metrics| metrics.sample());
                    if let Some((cpu, memory)) = sample {
                        self.record_sample(cpu, memory);
                        if bounded_percent(memory).is_some_and(|used| used >= MEMORY_EMERGENCY_PERCENT)
                            && emergency.is_empty()
                        {
                            let handler = self.inner.memory_pressure_handler.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            if let Some(handler) = handler {
                                // Slow systemd cleanup must not block capacity sampling or requests.
                                // One owned worker prevents duplicate cancellation of the same run.
                                emergency.spawn_blocking(move || handler());
                            }
                        }
                    }
                }
                completed = emergency.join_next(), if !emergency.is_empty() => {
                    if let Some(Err(error)) = completed {
                        error!(%error, "memory emergency worker failed");
                    }
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        error!(%error, "capacity connection task failed");
                    }
                }
            }
        };
        drop(listener);
        self.stop();
        while let Some(completed) = emergency.join_next().await {
            if let Err(error) = completed {
                error!(%error, "memory emergency worker failed during shutdown");
            }
        }
        let _ = tokio::fs::remove_file(&self.inner.socket_path).await;
        while let Some(completed) = connections.join_next().await {
            if let Err(error) = completed {
                error!(%error, "capacity task failed during shutdown");
            }
        }
        result
    }

    async fn serve_connection(&self, mut stream: UnixStream) -> std::io::Result<()> {
        let peer_uid = stream.peer_cred()?.uid();
        let raw = match timeout(READ_DEADLINE, read_line(&mut stream)).await {
            Ok(Ok(raw)) => raw,
            _ => {
                send_denied(&mut stream, "invalid acquire request").await;
                return Ok(());
            }
        };
        let request: AcquireRequest = match serde_json::from_slice::<AcquireRequest>(&raw) {
            Ok(request)
                if request.schema == CAPACITY_PROTOCOL_SCHEMA
                    && request.action == "acquire"
                    && valid_run_id(&request.run_id)
                    && !request.leaf_id.is_empty()
                    && request.leaf_id.len() <= 512 =>
            {
                request
            }
            _ => {
                send_denied(&mut stream, "invalid acquire request").await;
                return Ok(());
            }
        };
        let pending_id = match self.enqueue(&request.run_id, &request.leaf_id, peer_uid) {
            Ok(pending_id) => pending_id,
            Err(_) => {
                send_denied(&mut stream, "run identity is unavailable").await;
                return Ok(());
            }
        };
        let mut early_input = [0_u8; 4096];
        let mut ownership = ConnectionPermit {
            broker: self.clone(),
            pending_id,
            permit_id: None,
        };
        let grant = loop {
            let notified = self.inner.notify.notified();
            match self.take_outcome(pending_id).map_err(protocol_io)? {
                Some(PendingOutcome::Granted(grant)) => break grant,
                Some(PendingOutcome::Denied(message)) => {
                    send_denied(&mut stream, &message).await;
                    return Ok(());
                }
                Some(PendingOutcome::Waiting) | None => {}
            }
            tokio::select! {
                _ = notified => {}
                read = stream.read(&mut early_input) => {
                    let _ = read?;
                    self.cancel_pending(pending_id).map_err(protocol_io)?;
                    return Ok(());
                }
            }
        };
        let response = BrokerResponse {
            schema: CAPACITY_PROTOCOL_SCHEMA,
            status: "granted",
            permit_id: Some(&grant.permit_id),
            learned_capacity: Some(grant.learned_capacity),
            effective_capacity: Some(grant.effective_capacity),
            waited: Some(grant.waited),
            error: None,
        };
        ownership.permit_id = Some(grant.permit_id.clone());
        write_response(&mut stream, &response).await?;

        let mut release = [0_u8; 4096];
        loop {
            let notified = self.inner.notify.notified();
            if !self.is_active(&grant.permit_id).map_err(protocol_io)? {
                break;
            }
            tokio::select! {
                _ = notified => {}
                read = stream.read(&mut release) => {
                    let read = read?;
                    if read == 0 || release[..read].windows(18).any(|part| part == b"\"action\":\"release\"") {
                        break;
                    }
                }
            }
        }
        self.release(&grant.permit_id).map_err(protocol_io)?;
        Ok(())
    }

    fn enqueue(&self, run_id: &str, _leaf_id: &str, uid: u32) -> Result<u64, ProtocolError> {
        let now = self.inner.monotonic.seconds();
        let pending_id = {
            let mut state = self.state()?;
            if state.stopping || state.registered_runs.get(run_id) != Some(&uid) {
                return Err(ProtocolError::new(
                    ErrorCode::PermissionDenied,
                    "run identity is unavailable",
                ));
            }
            let pending_id = state.next_pending_id;
            state.next_pending_id = state.next_pending_id.wrapping_add(1).max(1);
            if state.queues.get(run_id).is_none_or(VecDeque::is_empty) {
                state.round_robin.push_back(run_id.to_owned());
            }
            state
                .queues
                .entry(run_id.to_owned())
                .or_default()
                .push_back(pending_id);
            state.pending.insert(
                pending_id,
                Pending {
                    run_id: run_id.to_owned(),
                    waited: false,
                    outcome: PendingOutcome::Waiting,
                },
            );
            state.start_epoch(run_id, now);
            state.grant_ready(self.inner.random.as_ref())?;
            if let Some(pending) = state.pending.get_mut(&pending_id)
                && matches!(pending.outcome, PendingOutcome::Waiting)
            {
                pending.waited = true;
            }
            pending_id
        };
        self.inner.notify.notify_waiters();
        Ok(pending_id)
    }

    fn take_outcome(&self, pending_id: u64) -> Result<Option<PendingOutcome>, ProtocolError> {
        let mut state = self.state()?;
        let outcome = state
            .pending
            .get(&pending_id)
            .map(|pending| pending.outcome.clone());
        if outcome
            .as_ref()
            .is_some_and(|outcome| !matches!(outcome, PendingOutcome::Waiting))
        {
            state.pending.remove(&pending_id);
        }
        Ok(outcome)
    }

    fn cancel_pending(&self, pending_id: u64) -> Result<(), ProtocolError> {
        let now = self.inner.monotonic.seconds();
        let mut state = self.state()?;
        let Some(pending) = state.pending.remove(&pending_id) else {
            return Ok(());
        };
        if let PendingOutcome::Granted(grant) = &pending.outcome {
            state.active.remove(&grant.permit_id);
        }
        if let Some(queue) = state.queues.get_mut(&pending.run_id) {
            queue.retain(|candidate| *candidate != pending_id);
            if queue.is_empty() {
                state.queues.remove(&pending.run_id);
                state
                    .round_robin
                    .retain(|candidate| candidate != &pending.run_id);
            }
        }
        state.finish_run(&pending.run_id, now);
        state.grant_ready(self.inner.random.as_ref())?;
        let adjustment = state.finish_epoch(now, self.inner.min_epoch_seconds);
        let learned = state.learned;
        let cap = state.cap;
        drop(state);
        if let Some(adjustment) = adjustment {
            self.persist_adjustment(learned, cap, &adjustment)?;
        }
        self.inner.notify.notify_waiters();
        Ok(())
    }

    fn release(&self, permit_id: &str) -> Result<(), ProtocolError> {
        let now = self.inner.monotonic.seconds();
        let (adjustment, learned, cap) = {
            let mut state = self.state()?;
            let Some(permit) = state.active.remove(permit_id) else {
                return Ok(());
            };
            state.finish_run(&permit.run_id, now);
            state.grant_ready(self.inner.random.as_ref())?;
            let adjustment = state.finish_epoch(now, self.inner.min_epoch_seconds);
            (adjustment, state.learned, state.cap)
        };
        if let Some(adjustment) = adjustment {
            self.persist_adjustment(learned, cap, &adjustment)?;
        }
        self.inner.notify.notify_waiters();
        Ok(())
    }

    fn is_active(&self, permit_id: &str) -> Result<bool, ProtocolError> {
        Ok(self.state()?.active.contains_key(permit_id))
    }

    fn stop(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.stopping = true;
            for pending in state.pending.values_mut() {
                if matches!(pending.outcome, PendingOutcome::Waiting) {
                    pending.outcome = PendingOutcome::Denied("broker shutting down".into());
                }
            }
            state.queues.clear();
            state.round_robin.clear();
            state.active.clear();
        }
        self.inner.notify.notify_waiters();
    }

    fn persist_adjustment(
        &self,
        learned: u32,
        cap: Option<u32>,
        adjustment: &Adjustment,
    ) -> Result<(), ProtocolError> {
        let at = timestamp(self.inner.clock.as_ref())?;
        let adjustment = adjustment.clone();
        self.inner
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE test_capacity_state SET learned_capacity=?1,cap=?2,updated_at=?3 WHERE singleton=1",
                    rusqlite::params![learned, cap, at],
                )?;
                transaction.execute(
                    "INSERT INTO test_capacity_events(at,actor,reason,previous_capacity,new_capacity,cap,p95_cpu_percent,p95_memory_percent,saturation_fraction,epoch_seconds) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![
                        at,
                        adjustment.actor,
                        adjustment.reason,
                        adjustment.previous,
                        adjustment.new,
                        cap,
                        adjustment.cpu,
                        adjustment.memory,
                        adjustment.saturation,
                        adjustment.duration,
                    ],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, ProtocolError> {
        self.inner.state.lock().map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "capacity state lock is unavailable",
            )
        })
    }
}

impl State {
    fn new(learned: u32, cap: Option<u32>) -> Self {
        Self {
            learned,
            cap,
            registered_runs: BTreeMap::new(),
            queues: BTreeMap::new(),
            round_robin: VecDeque::new(),
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
            next_pending_id: 1,
            stopping: false,
            paused: false,
            memory_emergency: false,
            cpu_pressure_streak: 0,
            memory_pressure_streak: 0,
            recovery_streak: 0,
            pending_decrease: false,
            epoch_memory_emergency: false,
            epoch_started: None,
            epoch_runs: BTreeSet::new(),
            samples: Vec::new(),
            run_started: BTreeMap::new(),
            run_durations: Vec::new(),
        }
    }

    fn effective(&self) -> u32 {
        self.cap.map_or(self.learned, |cap| self.learned.min(cap))
    }

    fn waiting(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }

    fn start_epoch(&mut self, run_id: &str, now: f64) {
        if self.epoch_started.is_none() {
            self.epoch_started = Some(now);
            self.samples.clear();
            self.run_durations.clear();
            self.pending_decrease = false;
            self.epoch_memory_emergency = false;
            self.epoch_runs.clear();
        }
        self.epoch_runs.insert(run_id.to_owned());
        self.run_started.entry(run_id.to_owned()).or_insert(now);
    }

    fn grant_ready(&mut self, random: &dyn RandomSource) -> Result<(), ProtocolError> {
        while !self.paused && self.active.len() < self.effective() as usize {
            let Some(run_id) = self.round_robin.pop_front() else {
                break;
            };
            let Some(queue) = self.queues.get_mut(&run_id) else {
                continue;
            };
            let Some(pending_id) = queue.pop_front() else {
                self.queues.remove(&run_id);
                continue;
            };
            if queue.is_empty() {
                self.queues.remove(&run_id);
            } else {
                self.round_robin.push_back(run_id.clone());
            }
            if !self.registered_runs.contains_key(&run_id) {
                if let Some(pending) = self.pending.get_mut(&pending_id) {
                    pending.outcome = PendingOutcome::Denied("run is no longer active".into());
                }
                continue;
            }
            let permit_id = unique_permit_id(random, &self.active)?;
            let effective_capacity = self.effective();
            if let Some(pending) = self.pending.get_mut(&pending_id) {
                let grant = Grant {
                    permit_id: permit_id.clone(),
                    learned_capacity: self.learned,
                    effective_capacity,
                    waited: pending.waited,
                };
                self.active.insert(
                    permit_id,
                    ActivePermit {
                        run_id: pending.run_id.clone(),
                    },
                );
                pending.outcome = PendingOutcome::Granted(grant);
            }
        }
        Ok(())
    }

    fn record_sample(&mut self, cpu: Option<f64>, memory: Option<f64>) {
        if memory.is_some_and(|value| value >= MEMORY_EMERGENCY_PERCENT) {
            self.memory_emergency = true;
            self.epoch_memory_emergency = true;
            self.paused = true;
            self.pending_decrease = true;
        }
        if self.epoch_started.is_none() && !self.memory_emergency {
            return;
        }
        let saturated = self.waiting() > 0
            || (!self.active.is_empty() && self.active.len() >= self.effective() as usize);
        if self.epoch_started.is_some() {
            self.samples.push((cpu, memory, saturated));
        }
        self.cpu_pressure_streak = if cpu.is_some_and(|value| value >= PRESSURE_PERCENT) {
            self.cpu_pressure_streak.saturating_add(1)
        } else {
            0
        };
        self.memory_pressure_streak = if memory.is_some_and(|value| value >= PRESSURE_PERCENT) {
            self.memory_pressure_streak.saturating_add(1)
        } else {
            0
        };
        if self.cpu_pressure_streak >= 4 || self.memory_pressure_streak >= 4 {
            self.paused = true;
            self.pending_decrease = true;
        }
        if self.paused
            && cpu.is_some_and(|value| value < RECOVERY_PERCENT)
            && memory.is_some_and(|value| {
                value
                    < if self.memory_emergency {
                        MEMORY_RECOVERY_PERCENT
                    } else {
                        RECOVERY_PERCENT
                    }
            })
        {
            self.recovery_streak = self.recovery_streak.saturating_add(1);
        } else if self.paused {
            self.recovery_streak = 0;
        }
        if self.paused && self.recovery_streak >= 2 {
            self.paused = false;
            self.memory_emergency = false;
            self.cpu_pressure_streak = 0;
            self.memory_pressure_streak = 0;
            self.recovery_streak = 0;
        }
    }

    fn finish_run(&mut self, run_id: &str, now: f64) {
        if !self.run_started.contains_key(run_id)
            || self.queues.contains_key(run_id)
            || self.active.values().any(|permit| permit.run_id == run_id)
            || self.registered_runs.contains_key(run_id)
        {
            return;
        }
        if let Some(started) = self.run_started.remove(run_id) {
            self.run_durations.push((now - started).max(0.0));
        }
    }

    fn finish_epoch(&mut self, now: f64, minimum: f64) -> Option<Adjustment> {
        let started = self.epoch_started?;
        let busy = !self.active.is_empty()
            || self.waiting() > 0
            || self
                .epoch_runs
                .iter()
                .any(|run| self.registered_runs.contains_key(run));
        let longest_run = self.run_durations.iter().copied().fold(0.0, f64::max);
        let duration = (now - started).max(0.0);
        // Evaluate a full live workload epoch as well as completed long runs.
        // Otherwise one long, underused permit can strand the queue forever.
        // An idle/registered-only queue is not evidence of executing work.
        let eligible = longest_run >= minimum || (!self.active.is_empty() && duration >= minimum);
        if busy && !eligible {
            return None;
        }
        let cpu_values = self
            .samples
            .iter()
            .filter_map(|sample| sample.0)
            .collect::<Vec<_>>();
        let memory_values = self
            .samples
            .iter()
            .filter_map(|sample| sample.1)
            .collect::<Vec<_>>();
        let complete = !self.samples.is_empty()
            && cpu_values.len() == self.samples.len()
            && memory_values.len() == self.samples.len();
        let cpu = percentile95(&cpu_values);
        let memory = percentile95(&memory_values);
        let saturation = (!self.samples.is_empty()).then(|| {
            self.samples.iter().filter(|sample| sample.2).count() as f64 / self.samples.len() as f64
        });
        let previous = self.learned;
        let mut reason = None;
        let mut new = previous;
        if eligible {
            if self.pending_decrease && previous > 1 {
                new = (previous - 1).min(previous.saturating_mul(3) / 4).max(1);
                reason = Some(if self.epoch_memory_emergency {
                    "memory_emergency"
                } else {
                    "sustained_pressure"
                });
            } else if !self.pending_decrease
                && !self.paused
                && complete
                && self.cap.is_none_or(|cap| cap > previous)
                && saturation.is_some_and(|value| value >= 0.5)
                && cpu.is_some_and(|value| value < UNDERUSED_PERCENT)
                && memory.is_some_and(|value| value < UNDERUSED_PERCENT)
            {
                new = (previous + 1)
                    .max(previous.saturating_mul(5).div_ceil(4))
                    .min(CAP_MAX);
                reason = Some("underused_saturated_epoch");
            }
        }
        let adjustment = reason.filter(|_| new != previous).map(|reason| {
            self.learned = new;
            Adjustment {
                actor: "system:auto".into(),
                reason: reason.into(),
                previous,
                new,
                cpu,
                memory,
                saturation,
                duration: Some(duration),
            }
        });
        if busy {
            self.epoch_started = Some(now);
            self.epoch_runs
                .retain(|run| self.registered_runs.contains_key(run));
            // Do not credit the same elapsed time in a later learning period.
            // Keep live ownership and queues; only reset their observation age.
            for started in self.run_started.values_mut() {
                *started = now;
            }
        } else {
            self.epoch_started = None;
            self.epoch_runs.clear();
            self.run_started.clear();
        }
        self.samples.clear();
        self.run_durations.clear();
        self.pending_decrease = false;
        self.epoch_memory_emergency = false;
        // A live epoch boundary is not pressure recovery. Keep the pause until
        // its existing two low-utilization samples have actually arrived.
        if !busy {
            self.cpu_pressure_streak = 0;
            self.memory_pressure_streak = 0;
            self.recovery_streak = 0;
            self.paused = self.memory_emergency;
        }
        adjustment
    }
}

async fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_socket() => tokio::fs::remove_file(path).await,
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "capacity socket path is occupied by a non-socket",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn read_line(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut block = [0_u8; 1024];
    loop {
        let read = stream.read(&mut block).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "capacity request closed before newline",
            ));
        }
        if let Some(newline) = block[..read].iter().position(|byte| *byte == b'\n') {
            if newline + 1 != read {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "capacity request contains trailing data",
                ));
            }
            result.extend_from_slice(&block[..newline]);
            return (result.len() <= MAX_REQUEST_BYTES)
                .then_some(result)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "capacity request is too large",
                    )
                });
        }
        result.extend_from_slice(&block[..read]);
        if result.len() > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "capacity request is too large",
            ));
        }
    }
}

async fn send_denied(stream: &mut UnixStream, message: &str) {
    let response = BrokerResponse {
        schema: CAPACITY_PROTOCOL_SCHEMA,
        status: "denied",
        permit_id: None,
        learned_capacity: None,
        effective_capacity: None,
        waited: None,
        error: Some(message),
    };
    let _ = write_response(stream, &response).await;
}

async fn write_response(
    stream: &mut UnixStream,
    response: &BrokerResponse<'_>,
) -> std::io::Result<()> {
    let mut encoded = serde_json::to_vec(response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    encoded.push(b'\n');
    timeout(WRITE_DEADLINE, stream.write_all(&encoded))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "capacity response timed out")
        })?
}

fn validate_run_id(run_id: &str) -> Result<(), ProtocolError> {
    if valid_run_id(run_id) {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "run_id is invalid",
        ))
    }
}

fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && run_id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && b"._-".contains(&byte))
        })
}

fn unique_permit_id(
    random: &dyn RandomSource,
    active: &BTreeMap<String, ActivePermit>,
) -> Result<String, ProtocolError> {
    for _ in 0..4 {
        let mut bytes = [0_u8; 8];
        random.fill(&mut bytes).map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot create a capacity permit")
                .with_detail(error.to_string())
        })?;
        let mut id = String::from("c");
        use std::fmt::Write;
        for byte in bytes {
            write!(&mut id, "{byte:02x}").expect("String writes cannot fail");
        }
        if !active.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(ProtocolError::new(
        ErrorCode::InternalError,
        "cannot allocate a unique capacity permit",
    ))
}

fn cpu_totals(root: &Path) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(root.join("stat")).ok()?;
    let values = text.lines().next()?.split_whitespace().collect::<Vec<_>>();
    if values.first().copied() != Some("cpu") || values.len() < 5 {
        return None;
    }
    let numbers = values[1..]
        .iter()
        .map(|value| value.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let total = numbers
        .iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(*value))?;
    let idle = numbers
        .get(3)?
        .checked_add(numbers.get(4).copied().unwrap_or(0))?;
    Some((total, idle))
}

fn memory_percent(root: &Path) -> Option<f64> {
    HostMemory::read(root)?.percent_used()
}

fn bounded_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

fn percentile95(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let index = ordered
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    ordered.get(index).copied()
}

fn timestamp(clock: &dyn Clock) -> Result<String, ProtocolError> {
    clock.now_utc().format(TIMESTAMP_FORMAT).map_err(|error| {
        ProtocolError::new(ErrorCode::InternalError, "cannot format capacity timestamp")
            .with_detail(error.to_string())
    })
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(
            ErrorCode::InternalError,
            "capacity database operation failed",
        )
        .with_detail(other.to_string()),
    }
}

fn protocol_io(error: ProtocolError) -> std::io::Error {
    std::io::Error::other(format!("{}: {}", error.code, error.message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_executor_core::{PermitProvider, PermitRequest, UnixPermitProvider};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::tempdir;
    use time::macros::datetime;

    struct ManualMonotonic(AtomicU64);

    impl ManualMonotonic {
        fn new() -> Self {
            Self(AtomicU64::new(0))
        }

        fn advance(&self, seconds: u64) {
            self.0.fetch_add(seconds * 1000, Ordering::SeqCst);
        }
    }

    impl MonotonicClock for ManualMonotonic {
        fn seconds(&self) -> f64 {
            self.0.load(Ordering::SeqCst) as f64 / 1000.0
        }
    }

    struct SequenceRandom(AtomicU64);

    impl RandomSource for SequenceRandom {
        fn fill(&self, destination: &mut [u8]) -> Result<(), getrandom::Error> {
            let value = self.0.fetch_add(1, Ordering::SeqCst).to_be_bytes();
            destination.copy_from_slice(&value[..destination.len()]);
            Ok(())
        }
    }

    struct NoMetrics;
    impl Metrics for NoMetrics {
        fn sample(&mut self) -> (Option<f64>, Option<f64>) {
            (None, None)
        }
    }

    fn make_broker(
        database: Database,
        socket: PathBuf,
        monotonic: Arc<ManualMonotonic>,
    ) -> CapacityBroker {
        CapacityBroker::with_adapters(
            database,
            socket,
            4,
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
            monotonic,
            Arc::new(SequenceRandom(AtomicU64::new(1))),
            Box::new(NoMetrics),
            Duration::from_secs(3600),
            600.0,
        )
        .expect("broker")
    }

    #[test]
    fn caps_fairness_pressure_recovery_and_learning_are_deterministic() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database.clone(),
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker.set_cap(Some(1), "uid:1").expect("cap");
        broker.register_run("run-a", 1000).expect("run a");
        broker.register_run("run-b", 1000).expect("run b");
        let a1 = broker.enqueue("run-a", "a1", 1000).expect("a1");
        let a2 = broker.enqueue("run-a", "a2", 1000).expect("a2");
        let b1 = broker.enqueue("run-b", "b1", 1000).expect("b1");
        let first = match broker.take_outcome(a1).expect("outcome") {
            Some(PendingOutcome::Granted(grant)) => grant,
            other => panic!("unexpected first outcome: {other:?}"),
        };
        assert!(matches!(
            broker.take_outcome(a2).expect("a2 waiting"),
            Some(PendingOutcome::Waiting)
        ));
        broker.release(&first.permit_id).expect("release a1");
        let second = match broker.take_outcome(a2).expect("outcome") {
            Some(PendingOutcome::Granted(grant)) => grant,
            other => panic!("unexpected second outcome: {other:?}"),
        };
        assert!(second.waited);
        broker.release(&second.permit_id).expect("release a2");
        let third = match broker.take_outcome(b1).expect("outcome") {
            Some(PendingOutcome::Granted(grant)) => grant,
            other => panic!("unexpected third outcome: {other:?}"),
        };
        assert_eq!(third.effective_capacity, 1);

        for _ in 0..4 {
            broker.record_sample(Some(99.0), Some(20.0));
        }
        assert!(broker.snapshot().expect("paused").paused);
        broker.record_sample(Some(50.0), Some(50.0));
        broker.record_sample(Some(50.0), Some(50.0));
        assert!(!broker.snapshot().expect("recovered").paused);

        monotonic.advance(601);
        broker.release(&third.permit_id).expect("release b1");
        broker.unregister_run("run-a").expect("finish a");
        broker.unregister_run("run-b").expect("finish b");
        assert_eq!(broker.snapshot().expect("learned").learned_capacity, 6);
        let persisted: u32 = database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT learned_capacity FROM test_capacity_state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("persisted");
        assert_eq!(persisted, 6);
    }

    #[test]
    fn memory_emergency_is_immediate_and_survives_the_last_run() {
        let temporary = tempdir().unwrap();
        let broker = make_broker(
            Database::open(temporary.path().join("authority.sqlite3")).unwrap(),
            temporary.path().join("capacity.sock"),
            Arc::new(ManualMonotonic::new()),
        );
        broker.record_sample(None, Some(89.99));
        assert!(!broker.snapshot().unwrap().paused);
        broker.record_sample(None, Some(90.0));
        assert!(broker.snapshot().unwrap().paused);
        broker.register_run("pressure-run", 1000).unwrap();
        let queued = broker.enqueue("pressure-run", "work", 1000).unwrap();
        assert!(matches!(
            broker.take_outcome(queued).unwrap(),
            Some(PendingOutcome::Waiting)
        ));
        broker.unregister_run("pressure-run").unwrap();
        assert!(
            broker.snapshot().unwrap().paused,
            "run completion must not reopen memory admission"
        );
        broker.record_sample(Some(20.0), Some(85.0));
        broker.record_sample(Some(20.0), Some(85.0));
        assert!(broker.snapshot().unwrap().paused);
        broker.record_sample(Some(20.0), Some(84.0));
        broker.record_sample(None, Some(84.0));
        assert!(
            broker.snapshot().unwrap().paused,
            "missing evidence cannot confirm recovery"
        );
        broker.record_sample(Some(20.0), Some(84.0));
        assert!(broker.snapshot().unwrap().paused);
        broker.record_sample(Some(20.0), Some(84.0));
        assert!(!broker.snapshot().unwrap().paused);
    }

    #[test]
    fn host_memory_uses_available_instead_of_free_and_rejects_invalid_measurements() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("meminfo");
        std::fs::write(
            &path,
            "MemTotal: 1000 kB\nMemFree: 1 kB\nMemAvailable: 200 kB\nCached: 199 kB\n",
        )
        .unwrap();
        let memory = HostMemory::read(temporary.path()).unwrap();
        assert_eq!(memory.total, 1_024_000);
        assert_eq!(memory.percent_used(), Some(80.0));
        assert!(!memory.emergency());
        for contents in [
            "MemTotal: 1000 kB\nMemFree: 1 kB\n",
            "MemTotal: 0 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 1000 kB\nMemAvailable: 1001 kB\n",
            "MemTotal: 1000 kB\nMemAvailable: invalid kB\n",
            "MemTotal: 1000 kB\nMemAvailable: 10 MB\n",
            "MemTotal: 1000 kB\nMemAvailable: 10 kB\nMemAvailable: 999 kB\n",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 0 kB\n",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                HostMemory::read(temporary.path()).is_none(),
                "invalid sample: {contents}"
            );
        }
    }

    #[test]
    fn a_late_memory_emergency_cannot_teach_an_underused_epoch_to_grow() {
        let temporary = tempdir().unwrap();
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            Database::open(temporary.path().join("authority.sqlite3")).unwrap(),
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker.register_run("late-growth", 1000).unwrap();
        for index in 0..9 {
            broker
                .enqueue("late-growth", &format!("leaf-{index}"), 1000)
                .unwrap();
        }
        for _ in 0..40 {
            broker.record_sample(Some(20.0), Some(20.0));
        }
        broker.record_sample(Some(20.0), Some(91.0));
        broker.record_sample(Some(20.0), Some(20.0));
        broker.record_sample(Some(20.0), Some(20.0));
        monotonic.advance(601);
        broker.unregister_run("late-growth").unwrap();
        let state = broker.snapshot().unwrap();
        assert_eq!(state.learned_capacity, 6);
        assert_eq!(state.last_adjustment.unwrap().reason, "memory_emergency");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emergency_cleanup_is_single_flight_and_does_not_block_sampling() {
        struct SampledPressure {
            cpu: Arc<AtomicU64>,
            percent: Arc<AtomicU64>,
            sampled: std::sync::mpsc::Sender<()>,
        }
        impl Metrics for SampledPressure {
            fn sample(&mut self) -> (Option<f64>, Option<f64>) {
                let value = self.percent.load(Ordering::SeqCst);
                let _ = self.sampled.send(());
                (
                    Some(self.cpu.load(Ordering::SeqCst) as f64),
                    Some(value as f64),
                )
            }
        }
        let temporary = tempdir().unwrap();
        let cpu = Arc::new(AtomicU64::new(99));
        let percent = Arc::new(AtomicU64::new(40));
        let (sampled, samples) = std::sync::mpsc::channel();
        let broker = CapacityBroker::with_adapters(
            Database::open(temporary.path().join("authority.sqlite3")).unwrap(),
            temporary.path().join("capacity.sock"),
            2,
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-03 12:00 UTC))),
            Arc::new(ManualMonotonic::new()),
            Arc::new(SequenceRandom(AtomicU64::new(1))),
            Box::new(SampledPressure {
                cpu: cpu.clone(),
                percent: percent.clone(),
                sampled,
            }),
            Duration::from_millis(25),
            600.0,
        )
        .unwrap();
        broker.register_run("cpu-only", 1000).unwrap();
        broker.enqueue("cpu-only", "active", 1000).unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let (entered, entry) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Arc::new(Mutex::new(released));
        let counted = calls.clone();
        broker.set_memory_pressure_handler(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            entered.send(()).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }));
        let (shutdown, receiver) = watch::channel(false);
        let server = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.serve(receiver).await })
        };
        let samples = tokio::task::spawn_blocking(move || {
            for _ in 0..5 {
                samples.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            samples
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "CPU saturation alone must not terminate work"
        );
        assert!(broker.snapshot().unwrap().paused);
        percent.store(92, Ordering::SeqCst);
        tokio::task::spawn_blocking(move || entry.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .unwrap();
        let samples = tokio::task::spawn_blocking(move || {
            for _ in 0..5 {
                samples.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            samples
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(broker.snapshot().unwrap().paused);
        percent.store(40, Ordering::SeqCst);
        cpu.store(20, Ordering::SeqCst);
        release.send(()).unwrap();
        tokio::task::spawn_blocking(move || {
            for _ in 0..3 {
                samples.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .await
        .unwrap();
        shutdown.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!broker.state().unwrap().memory_emergency);
    }

    #[test]
    fn a_complete_saturated_underused_epoch_increases_and_survives_restart() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database.clone(),
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker.register_run("run-grow", 1000).expect("run");
        let pending = (0..9)
            .map(|index| {
                broker
                    .enqueue("run-grow", &format!("leaf-{index}"), 1000)
                    .expect("enqueue")
            })
            .collect::<Vec<_>>();
        assert_eq!(broker.snapshot().expect("saturated").active, 8);
        for _ in 0..4 {
            broker.record_sample(Some(40.0), Some(40.0));
        }
        monotonic.advance(601);
        broker.unregister_run("run-grow").expect("finish run");
        assert_eq!(broker.snapshot().expect("grown").learned_capacity, 10);
        // Consume the denied waiting outcome so this fixture also proves an
        // unregister never strands a queued connection.
        assert!(pending.into_iter().any(|pending_id| matches!(
            broker.take_outcome(pending_id).expect("outcome"),
            Some(PendingOutcome::Denied(_))
        )));

        let restarted = make_broker(
            database,
            temporary.path().join("capacity-restarted.sock"),
            Arc::new(ManualMonotonic::new()),
        );
        assert_eq!(restarted.snapshot().expect("restart").learned_capacity, 10);
    }

    #[test]
    fn completed_underused_run_can_grow_with_another_run_queued() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database,
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker
            .register_run("run-finished", 1000)
            .expect("first run");
        broker
            .register_run("run-still-queued", 1000)
            .expect("second run");
        for index in 0..9 {
            broker
                .enqueue("run-finished", &format!("leaf-{index}"), 1000)
                .expect("first leaf");
        }
        let pending = broker
            .enqueue("run-still-queued", "waiting-leaf", 1000)
            .expect("queued leaf");
        for _ in 0..4 {
            broker.record_sample(Some(40.0), Some(40.0));
        }
        monotonic.advance(601);
        broker
            .unregister_run("run-finished")
            .expect("finish eligible run");
        let snapshot = broker.snapshot().expect("snapshot");
        assert_eq!(
            snapshot.learned_capacity, 10,
            "queued unrelated work must not prevent upward learning"
        );
        assert_eq!(
            snapshot.active, 1,
            "the unrelated run remains admitted, not cancelled"
        );
        assert!(matches!(
            broker.take_outcome(pending).expect("pending outcome"),
            Some(PendingOutcome::Granted(_))
        ));
        broker
            .unregister_run("run-still-queued")
            .expect("cleanup second run");
        assert_eq!(
            broker
                .snapshot()
                .expect("no double credit")
                .learned_capacity,
            10
        );
    }

    #[test]
    fn active_underused_epoch_recovers_without_waiting_for_a_run_to_finish() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database.clone(),
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        // Reproduce the existing learned floor without changing an admin cap:
        // one long-running leaf occupies the grant; a second run must wait.
        broker.state().expect("state").learned = 1;
        broker
            .register_run("long-active", 1000)
            .expect("active run");
        broker.register_run("waiting", 1000).expect("waiting run");
        let first = broker
            .enqueue("long-active", "long-leaf", 1000)
            .expect("first");
        let first_permit = match broker.take_outcome(first).expect("first outcome") {
            Some(PendingOutcome::Granted(permit)) => permit,
            _ => panic!("first leaf must be admitted"),
        };
        let second = broker
            .enqueue("waiting", "short-leaf", 1000)
            .expect("second");
        for _ in 0..40 {
            monotonic.advance(15);
            broker.record_sample(Some(40.0), Some(40.0));
        }
        let snapshot = broker.snapshot().expect("snapshot");
        assert_eq!(
            snapshot.learned_capacity, 2,
            "active underused work must not strand the queue"
        );
        assert_eq!(
            snapshot.active, 2,
            "the existing leaf is preserved and the waiting leaf is admitted"
        );
        assert_eq!(snapshot.waiting, 0);
        assert!(matches!(
            broker.take_outcome(second).expect("second outcome"),
            Some(PendingOutcome::Granted(_))
        ));
        assert!(
            broker
                .state()
                .expect("state")
                .active
                .contains_key(&first_permit.permit_id)
        );
        for _ in 0..10 {
            monotonic.advance(15);
            broker.record_sample(Some(40.0), Some(40.0));
        }
        assert_eq!(
            broker
                .snapshot()
                .expect("no double credit")
                .learned_capacity,
            2
        );
        broker.unregister_run("long-active").expect("finish first");
        broker.unregister_run("waiting").expect("finish second");
        assert_eq!(broker.snapshot().expect("complete").learned_capacity, 2);
        let restarted = make_broker(database, temporary.path().join("restarted.sock"), monotonic);
        assert_eq!(
            restarted
                .snapshot()
                .expect("persisted learning")
                .learned_capacity,
            2
        );
    }

    #[test]
    fn live_epochs_preserve_duration_complete_samples_saturation_and_caps() {
        for (ticks, cpu, memory, cap, saturated, missing_sample) in [
            (39, 40.0, 40.0, None, true, false),
            (40, 40.0, 40.0, None, true, true),
            (40, 95.0, 40.0, None, true, false),
            (40, 40.0, 99.0, None, true, false),
            (40, 40.0, 40.0, Some(1), true, false),
            (40, 40.0, 40.0, None, false, false),
        ] {
            let temporary = tempdir().expect("tempdir");
            let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
            let monotonic = Arc::new(ManualMonotonic::new());
            let broker = make_broker(
                database,
                temporary.path().join("capacity.sock"),
                monotonic.clone(),
            );
            let learned = if saturated { 1 } else { 8 };
            broker.state().expect("state").learned = learned;
            if cap.is_some() {
                broker.set_cap(cap, "test:admin").expect("cap");
            }
            broker.register_run("active", 1000).expect("active run");
            broker
                .enqueue("active", "long-leaf", 1000)
                .expect("active leaf");
            if saturated {
                broker.register_run("waiting", 1000).expect("waiting run");
                broker
                    .enqueue("waiting", "next-leaf", 1000)
                    .expect("waiting leaf");
            }
            for index in 0..ticks {
                monotonic.advance(15);
                let observed_cpu = if missing_sample && index == 20 {
                    None
                } else {
                    Some(cpu)
                };
                broker.record_sample(observed_cpu, Some(memory));
            }
            let snapshot = broker.snapshot().expect("snapshot");
            assert_eq!(
                snapshot.learned_capacity, learned,
                "no unsupported live-epoch growth"
            );
            assert_eq!(
                snapshot.active, 1,
                "no admitted work may be killed or extra work admitted"
            );
            assert_eq!(snapshot.waiting, u32::from(saturated));
            if memory >= 98.0 {
                assert!(
                    snapshot.paused,
                    "an epoch boundary must not clear sustained pressure"
                );
            }
            broker.unregister_run("active").expect("cleanup active");
            if saturated {
                broker.unregister_run("waiting").expect("cleanup waiting");
            }
        }
    }

    #[test]
    fn live_pressure_epoch_reduces_future_grants_without_stopping_active_work() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database,
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker.register_run("busy", 1000).expect("busy run");
        for index in 0..9 {
            broker
                .enqueue("busy", &format!("leaf-{index}"), 1000)
                .expect("leaf");
        }
        let original_permits = broker
            .state()
            .expect("state")
            .active
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        for _ in 0..40 {
            monotonic.advance(15);
            broker.record_sample(Some(99.0), Some(40.0));
        }
        let snapshot = broker.snapshot().expect("pressure snapshot");
        assert_eq!(snapshot.learned_capacity, 6);
        assert_eq!(snapshot.active, 8);
        assert_eq!(snapshot.waiting, 1);
        assert!(snapshot.paused);
        assert_eq!(
            broker
                .state()
                .expect("state")
                .active
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            original_permits
        );
        for _ in 0..2 {
            monotonic.advance(15);
            broker.record_sample(Some(40.0), Some(40.0));
        }
        let snapshot = broker.snapshot().expect("recovery snapshot");
        assert!(!snapshot.paused);
        assert_eq!(snapshot.learned_capacity, 6);
        assert_eq!(snapshot.active, 8);
        assert_eq!(snapshot.waiting, 1);
        broker.unregister_run("busy").expect("cleanup");
        assert_eq!(broker.snapshot().expect("complete").learned_capacity, 6);
    }

    #[test]
    fn registered_without_executing_work_does_not_manufacture_a_live_epoch() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(
            database,
            temporary.path().join("capacity.sock"),
            monotonic.clone(),
        );
        broker
            .register_run("registered-only", 1000)
            .expect("registered run");
        for _ in 0..80 {
            monotonic.advance(15);
            broker.record_sample(Some(10.0), Some(10.0));
        }
        let snapshot = broker.snapshot().expect("snapshot");
        assert_eq!(snapshot.learned_capacity, 8);
        assert_eq!(snapshot.active, 0);
        broker.unregister_run("registered-only").expect("cleanup");
    }

    #[test]
    fn backlog_learning_keeps_duration_sample_pressure_and_cap_guards() {
        for (seconds, cpu, memory, cap) in [
            (30, Some(40.0), Some(40.0), None),
            (601, None, Some(40.0), None),
            (601, Some(40.0), None, None),
            (601, Some(90.0), Some(40.0), None),
            (601, Some(40.0), Some(40.0), Some(8)),
        ] {
            let temporary = tempdir().expect("tempdir");
            let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
            let monotonic = Arc::new(ManualMonotonic::new());
            let broker = make_broker(
                database,
                temporary.path().join("capacity.sock"),
                monotonic.clone(),
            );
            broker.set_cap(cap, "test").expect("cap");
            broker.register_run("run-first", 1000).expect("first run");
            broker.register_run("run-other", 1000).expect("other run");
            for index in 0..8 {
                broker
                    .enqueue("run-first", &format!("leaf-{index}"), 1000)
                    .expect("first leaf");
            }
            broker
                .enqueue("run-other", "waiting", 1000)
                .expect("other leaf");
            for _ in 0..4 {
                broker.record_sample(cpu, memory);
            }
            monotonic.advance(seconds);
            broker.unregister_run("run-first").expect("finish first");
            assert_eq!(broker.snapshot().expect("guarded").learned_capacity, 8);
            assert_eq!(broker.snapshot().expect("other survives").active, 1);
            broker.unregister_run("run-other").expect("cleanup");
        }
    }

    #[tokio::test]
    async fn cancelled_connection_handler_releases_its_active_permit() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let broker = make_broker(
            database,
            temporary.path().join("capacity.sock"),
            Arc::new(ManualMonotonic::new()),
        );
        broker
            .register_run("run-cancel", rustix::process::getuid().as_raw())
            .expect("run");
        let (mut client, server) = UnixStream::pair().expect("pair");
        let serving = broker.clone();
        let handler = tokio::spawn(async move { serving.serve_connection(server).await });
        let request = serde_json::json!({"schema": CAPACITY_PROTOCOL_SCHEMA, "action": "acquire", "run_id": "run-cancel", "leaf_id": "check"});
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("request");
        let response = timeout(Duration::from_secs(2), read_line(&mut client))
            .await
            .expect("grant deadline")
            .expect("grant");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response).unwrap()["status"],
            "granted"
        );
        assert_eq!(broker.snapshot().unwrap().active, 1);
        handler.abort();
        assert!(handler.await.unwrap_err().is_cancelled());
        assert_eq!(broker.snapshot().unwrap().active, 0);
        assert!(broker.state().unwrap().pending.is_empty());
    }

    #[test]
    fn abandoned_untaken_grant_releases_capacity_for_the_next_request() {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let broker = make_broker(
            database,
            temporary.path().join("capacity.sock"),
            Arc::new(ManualMonotonic::new()),
        );
        broker.set_cap(Some(1), "uid:1").unwrap();
        broker.register_run("run-cancel", 1000).unwrap();
        let abandoned = broker.enqueue("run-cancel", "abandoned", 1000).unwrap();
        let following = broker.enqueue("run-cancel", "following", 1000).unwrap();
        assert!(matches!(
            broker.take_outcome(following).unwrap(),
            Some(PendingOutcome::Waiting)
        ));
        broker.cancel_pending(abandoned).unwrap();
        let Some(PendingOutcome::Granted(grant)) = broker.take_outcome(following).unwrap() else {
            panic!("next request was not admitted");
        };
        broker.release(&grant.permit_id).unwrap();
        assert_eq!(broker.snapshot().unwrap().active, 0);
        assert!(broker.state().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn unix_protocol_matches_executor_client_and_releases_on_disconnect() {
        let temporary = tempdir().expect("tempdir");
        let socket = temporary.path().join("capacity.sock");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let monotonic = Arc::new(ManualMonotonic::new());
        let broker = make_broker(database, socket.clone(), monotonic);
        let uid = rustix::process::getuid().as_raw();
        broker.register_run("run-ok", uid).expect("run");
        broker
            .register_run("run-wrong", uid + 1)
            .expect("wrong run");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let serving = broker.clone();
        let server = tokio::spawn(async move { serving.serve(shutdown_rx).await });
        let ready = timeout(Duration::from_secs(2), async {
            while !socket.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(ready.is_ok(), "capacity socket was not created");

        let provider = UnixPermitProvider::new(&socket).expect("provider");
        let mut invalid = UnixStream::connect(&socket).await.expect("invalid client");
        invalid.write_all(b"{}\n").await.expect("invalid request");
        let invalid_reply = read_line(&mut invalid).await.expect("denial");
        let invalid_reply: serde_json::Value =
            serde_json::from_slice(&invalid_reply).expect("denial JSON");
        assert_eq!(invalid_reply["status"], "denied");

        let permit = provider
            .acquire(PermitRequest {
                run_id: "run-ok".into(),
                leaf_id: "check/case".into(),
            })
            .await
            .expect("permit");
        assert_eq!(broker.snapshot().expect("active").active, 1);
        drop(permit);
        timeout(Duration::from_secs(2), async {
            loop {
                let notified = broker.inner.notify.notified();
                if broker.snapshot().expect("snapshot").active == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("disconnect release");

        let denied = match provider
            .acquire(PermitRequest {
                run_id: "run-wrong".into(),
                leaf_id: "check/case".into(),
            })
            .await
        {
            Ok(_) => panic!("wrong uid received a permit"),
            Err(error) => error,
        };
        assert!(denied.to_string().contains("run identity is unavailable"));
        let held_during_shutdown = provider
            .acquire(PermitRequest {
                run_id: "run-ok".into(),
                leaf_id: "held-during-shutdown".into(),
            })
            .await
            .expect("held permit");
        assert_eq!(broker.snapshot().expect("held").active, 1);
        shutdown_tx.send(true).expect("shutdown");
        timeout(Duration::from_secs(2), server)
            .await
            .expect("shutdown drained active connection")
            .expect("server task")
            .expect("server result");
        drop(held_during_shutdown);
        assert!(!socket.exists());
    }
}
