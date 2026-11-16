//! One bounded worker pool for periodic and explicitly requested maintenance.
//! Durable cursors and leases live in PostgreSQL; worker threads never occupy
//! the request runtime's blocking pool or request database connections.
pub mod jobs;
#[cfg(test)]
mod tests;
use crate::database::{Database, DatabaseError, params};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, watch};

#[derive(Clone, Debug)]
pub struct Budget {
    deadline: Instant,
    max_rows: u64,
    max_bytes: u64,
    rows: u64,
    bytes: u64,
    cancel: Arc<AtomicBool>,
}
impl Budget {
    pub fn new(duration: Duration, max_rows: u64, max_bytes: u64) -> Self {
        Self {
            deadline: Instant::now() + duration,
            max_rows,
            max_bytes,
            rows: 0,
            bytes: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn exhausted(&self) -> bool {
        self.cancelled()
            || Instant::now() >= self.deadline
            || self.rows >= self.max_rows
            || self.bytes >= self.max_bytes
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub fn consume(&mut self, rows: u64, bytes: u64) -> bool {
        self.rows = self.rows.saturating_add(rows);
        self.bytes = self.bytes.saturating_add(bytes);
        !self.exhausted()
    }
    pub fn rows(&self) -> u64 {
        self.rows
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobStatus {
    Complete,
    Deferred { not_before_ms: u64 },
    Continue { cursor: String },
    Retry { reason: String },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobResult {
    pub status: JobStatus,
    pub rows: u64,
    pub bytes: u64,
}
impl JobResult {
    pub fn complete() -> Self {
        Self {
            status: JobStatus::Complete,
            rows: 0,
            bytes: 0,
        }
    }
}
pub trait Job: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn priority(&self) -> i16 {
        0
    }
    fn trigger(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
    /// A successful inbound-event offset can survive completion or retry.
    fn checkpoint(&self) -> Option<String> {
        None
    }
    fn key(&self) -> String {
        self.name().into()
    }
    fn run(&self, budget: &mut Budget, cursor: Option<String>) -> JobResult;
}
struct Request {
    job: Arc<dyn Job>,
    duration: Duration,
    max_rows: u64,
    max_bytes: u64,
    cursor: Option<String>,
    interval: Option<Duration>,
    forced: bool,
    reply: oneshot::Sender<Result<JobResult, String>>,
}
struct Queue {
    items: VecDeque<Request>,
    closed: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    active: Mutex<BTreeSet<String>>,
    due: Mutex<BTreeMap<String, u64>>,
    changed: watch::Sender<()>,
}
/// Shared by the periodic loop and explicit storage jobs. Drop closes the
/// queue; shutdown cooperatively cancels each current bounded step.
#[derive(Clone)]
pub struct Scheduler {
    owner: Arc<()>,
    queue_capacity: usize,
    shared: Arc<Shared>,
}
#[derive(Clone)]
pub struct PeriodicJob {
    pub job: Arc<dyn Job>,
    /// Observe restart-sensitive runtime state promptly, retaining saved cursors
    /// and honoring any live lease before the first bounded step.
    pub run_on_start: bool,
    pub interval: Duration,
    pub budget: Duration,
    pub max_rows: u64,
    pub max_bytes: u64,
}
impl Scheduler {
    pub fn new(database: Database, workers: usize, queue_capacity: usize) -> Result<Self, String> {
        if workers == 0 || workers > 8 || queue_capacity == 0 || queue_capacity > 128 {
            return Err("invalid maintenance pool bounds".into());
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                items: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            workers: Mutex::new(Vec::new()),
            stop: Arc::new(AtomicBool::new(false)),
            active: Mutex::new(BTreeSet::new()),
            due: Mutex::new(BTreeMap::new()),
            changed: watch::channel(()).0,
        });
        for index in 0..workers {
            let worker_state = shared.clone();
            let database = database.clone();
            let worker = std::thread::Builder::new()
                .name(format!("dc2-maint-{index}"))
                .spawn(move || {
                    let shared=worker_state;
                    loop {
                        let request={
                            let mut queue=shared.queue.lock().unwrap_or_else(|p|p.into_inner());
                            while queue.items.is_empty() && !queue.closed {
                                queue=shared.ready.wait(queue).unwrap_or_else(|p|p.into_inner());
                            }
                            if queue.items.is_empty() {break;}
                            // Bounded priority queue, FIFO within each priority.
                            let priority=queue.items.iter().map(|r|r.job.priority()).max().unwrap();
                            let index=queue.items.iter().position(|r|r.job.priority()==priority).unwrap();
                            queue.items.remove(index).unwrap()
                        };
                        let key = request.job.key();
                        let result = crate::database::maintenance_context(|| {
                            run_step(&database, &shared, &request)
                        });
                        if result.is_err() && request.interval.is_some() {
                            shared.due.lock().unwrap_or_else(|p|p.into_inner()).insert(key.clone(),now_ms()+5000);
                        }
                        shared
                            .active
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&key);
                        if matches!(&result, Ok(result) if !matches!(result.status, JobStatus::Deferred {..})) {
                            shared.changed.send_replace(());
                        }
                        let _ = request.reply.send(result);
                    }
                })
                .map_err(|_| "maintenance worker could not start")?;
            shared
                .workers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(worker);
        }
        Ok(Self {
            owner: Arc::new(()),
            queue_capacity,
            shared,
        })
    }
    fn enqueue(
        &self,
        job: Arc<dyn Job>,
        budget: Budget,
        cursor: Option<String>,
        interval: Option<Duration>,
        forced: bool,
    ) -> Result<oneshot::Receiver<Result<JobResult, String>>, String> {
        if self.shared.stop.load(Ordering::Acquire) {
            return Err("maintenance scheduler is stopping".into());
        }
        let key = job.key();
        if key.is_empty()
            || key.len() > 160
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return Err("invalid maintenance job identity".into());
        }
        if !self
            .shared
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone())
        {
            return Err("maintenance job already scheduled".into());
        }
        let (reply, result) = oneshot::channel();
        let mut queue = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if queue.closed || queue.items.len() >= self.queue_capacity {
            self.shared
                .active
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&key);
            return Err("maintenance queue is full or stopping; job was not started".into());
        }
        queue.items.push_back(Request {
            job,
            duration: budget.remaining(),
            max_rows: budget.max_rows,
            max_bytes: budget.max_bytes,
            cursor,
            interval,
            forced,
            reply,
        });
        self.shared.ready.notify_one();
        Ok(result)
    }
    pub async fn submit_async(
        &self,
        job: Arc<dyn Job>,
        budget: Budget,
        cursor: Option<String>,
    ) -> Result<JobResult, String> {
        self.enqueue(job, budget, cursor, None, false)?
            .await
            .map_err(|_| "maintenance result unavailable")?
    }
    pub fn submit(
        &self,
        job: Arc<dyn Job>,
        budget: Budget,
        cursor: Option<String>,
    ) -> Result<JobResult, String> {
        self.enqueue(job, budget, cursor, None, false)?
            .blocking_recv()
            .map_err(|_| "maintenance result unavailable")?
    }
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed = true;
        self.shared.ready.notify_all();
        self.shared.changed.send_replace(());
    }
    pub fn retry_events(&self) -> watch::Receiver<()> {
        self.shared.changed.subscribe()
    }
    /// Wait for a real worker transition or an explicit retry/lease deadline.
    /// Neither wake source says the job completed: the caller must submit again
    /// and observe its durable result. Shutdown always interrupts the wait.
    pub async fn wait_retry(
        &self,
        events: &mut watch::Receiver<()>,
        not_before_ms: Option<u64>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> bool {
        if self.shared.stop.load(Ordering::Acquire) || *shutdown.borrow() {
            return false;
        }
        let deadline = not_before_ms.map_or(Duration::from_secs(1), |due| {
            Duration::from_millis(due.saturating_sub(now_ms()))
        });
        tokio::select! {
            _ = tokio::time::timeout(deadline, events.changed()) => {},
            _ = shutdown.changed() => return false,
        }
        !self.shared.stop.load(Ordering::Acquire) && !*shutdown.borrow()
    }
    pub fn shutdown_and_join(&self) {
        self.shutdown();
        let workers = std::mem::take(
            &mut *self
                .shared
                .workers
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for worker in workers {
            let _ = worker.join();
        }
    }
    pub async fn serve(&self, jobs: Vec<PeriodicJob>, mut shutdown: watch::Receiver<bool>) {
        let mut running = tokio::task::JoinSet::new();
        let mut signals = tokio::task::JoinSet::new();
        for spec in &jobs {
            if let Some(signal) = spec.job.trigger() {
                let key = spec.job.key();
                signals.spawn(async move {
                    signal.notified().await;
                    (key, signal)
                });
            }
        }
        let mut expedited = jobs
            .iter()
            .filter(|spec| spec.run_on_start)
            .map(|spec| spec.job.key())
            .collect::<BTreeSet<_>>();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break;}},
                _=tick.tick()=>{
                    let now=now_ms();
                    for spec in &jobs {
                        let key=spec.job.key();
                        let due=self.shared.due.lock().unwrap_or_else(|p|p.into_inner()).get(&key).copied().unwrap_or(0);
                        let forced=expedited.contains(&key);
                        if due>now&&!forced{continue;}
                        if let Ok(wait)=self.enqueue(spec.job.clone(),Budget::new(spec.budget,spec.max_rows,spec.max_bytes),None,Some(spec.interval),forced) {
                            expedited.remove(&key);
                            running.spawn(async move{let _=wait.await;});
                        }
                    }
                },
                completed=signals.join_next(),if !signals.is_empty()=>{
                    if let Some(Ok((key,signal)))=completed {
                        expedited.insert(key.clone());
                        signals.spawn(async move{signal.notified().await;(key,signal)});
                    }
                },
                _=running.join_next(),if !running.is_empty()=>{},
            }
        }
        signals.abort_all();
        while signals.join_next().await.is_some() {}
        self.shutdown();
        while running.join_next().await.is_some() {}
    }
}
impl Drop for Scheduler {
    fn drop(&mut self) {
        if Arc::strong_count(&self.owner) == 1 {
            self.shutdown();
        }
    }
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
fn run_step(database: &Database, shared: &Shared, request: &Request) -> Result<JobResult, String> {
    let key = request.job.key();
    let now = now_ms();
    let claimed=database.maintenance_transaction(|tx|{
        tx.execute("INSERT INTO coordinator_maintenance(job_key,due_at_ms,priority) VALUES($1,$2,$3) ON CONFLICT(job_key) DO UPDATE SET priority=excluded.priority",params![key,now,i64::from(request.job.priority())])?;
        let (cursor,due,lease,attempt):(Option<String>,u64,u64,u64)=tx.query_row("SELECT cursor,due_at_ms,COALESCE(lease_until_ms,0),attempts FROM coordinator_maintenance WHERE job_key=$1 FOR UPDATE",[&key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if (!request.forced&&due>now)||lease>now{return Ok(Err(due.max(lease)));}
        let generation=attempt.saturating_add(1);
        let lease_end=now.saturating_add(120_000);
        tx.execute("UPDATE coordinator_maintenance SET lease_until_ms=$1,attempts=$2,last_status='running' WHERE job_key=$3",params![lease_end,generation,key])?;
        Ok(Ok((cursor,generation)))
    }).map_err(|_|"maintenance claim unavailable".to_owned())?;
    let (stored_cursor, generation) = match claimed {
        Ok(value) => value,
        Err(due) => {
            if request.interval.is_some() {
                shared
                    .due
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(key, due);
            }
            return Ok(JobResult {
                status: JobStatus::Deferred { not_before_ms: due },
                rows: 0,
                bytes: 0,
            });
        }
    };
    let mut budget = Budget::new(request.duration, request.max_rows, request.max_bytes);
    budget.cancel = shared.stop.clone();
    // Cursor storage is JSONB but the job interface deliberately treats it as opaque.
    let stored_cursor = stored_cursor.and_then(|value| {
        serde_json::from_str::<Option<String>>(&value)
            .ok()
            .flatten()
    });
    let cursor = request.cursor.clone().or(stored_cursor);
    let result = if budget.cancelled() {
        JobResult {
            status: JobStatus::Continue {
                cursor: cursor.clone().unwrap_or_default(),
            },
            rows: 0,
            bytes: 0,
        }
    } else {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            request.job.run(&mut budget, cursor.clone())
        }))
        .unwrap_or(JobResult {
            status: JobStatus::Retry {
                reason: "job_panicked".into(),
            },
            rows: 0,
            bytes: 0,
        })
    };
    let (status, next, error, delay) = match &result.status {
        JobStatus::Complete => (
            "complete",
            request.job.checkpoint(),
            None,
            request.interval.unwrap_or(Duration::from_secs(3600)),
        ),
        JobStatus::Deferred { not_before_ms } => (
            "deferred",
            cursor,
            None,
            Duration::from_millis(not_before_ms.saturating_sub(now_ms())),
        ),
        JobStatus::Continue { cursor } => (
            "continuing",
            Some(cursor.clone()),
            None,
            Duration::from_millis(100),
        ),
        JobStatus::Retry { .. } => (
            "retry",
            request.job.checkpoint().or(cursor),
            Some("job_failed"),
            Duration::from_secs((1u64 << generation.min(6)).min(60)),
        ),
    };
    let due = now_ms().saturating_add(delay.as_millis() as u64);
    let encoded = serde_json::to_string(&next).map_err(|_| "maintenance cursor encoding failed")?;
    database.maintenance_transaction(|tx|{
        let changed=tx.execute("UPDATE coordinator_maintenance SET cursor=$1::jsonb,due_at_ms=$2,lease_until_ms=0,last_status=$3,last_rows=$4,last_bytes=$5,last_error_code=$6 WHERE job_key=$7 AND attempts=$8",params![encoded,due,status,result.rows,result.bytes,error,key,generation])?;
        if changed!=1{return Err(DatabaseError::Startup("maintenance lease changed".into()));}
        if request.interval.is_none() && matches!(result.status,JobStatus::Complete) {
            tx.execute("DELETE FROM coordinator_maintenance WHERE job_key=$1 AND attempts=$2",params![key,generation])?;
        }
        Ok(())
    }).map_err(|_|"maintenance result could not be saved".to_owned())?;
    if request.interval.is_some() {
        shared
            .due
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, due);
    }
    Ok(result)
}
