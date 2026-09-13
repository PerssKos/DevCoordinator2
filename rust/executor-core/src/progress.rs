//! Event-driven process admission observations; never a capacity controller.
use crate::{LeafSelector, capacity::CapacityObservation};
use devcoordinator2_executor_protocol::{ExecutionProgress, LeafStatus};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub(crate) struct ProcessUpdate {
    pub leaf_id: String,
    pub selector: LeafSelector,
    pub progress: ExecutionProgress,
    pub capacity: Option<CapacityObservation>,
    pub status: Option<LeafStatus>,
    pub started_mono: Option<Instant>,
    pub finished_mono: Option<Instant>,
}

pub(crate) struct ProgressReporter {
    sender: UnboundedSender<ProcessUpdate>,
    update: ProcessUpdate,
    queued: Instant,
    started: Option<Instant>,
}

impl ProgressReporter {
    pub fn new(
        sender: UnboundedSender<ProcessUpdate>,
        leaf_id: String,
        selector: LeafSelector,
    ) -> Self {
        let result = Self {
            sender,
            queued: Instant::now(),
            started: None,
            update: ProcessUpdate {
                leaf_id,
                selector,
                progress: ExecutionProgress {
                    waiting: 1,
                    queued_at_epoch_ms: now(),
                    ..Default::default()
                },
                capacity: None,
                status: None,
                started_mono: None,
                finished_mono: None,
            },
        };
        result.send();
        result
    }
    pub fn admitted(&mut self, capacity: CapacityObservation) {
        self.update.progress.waiting = 0;
        self.update.progress.admitted = 1;
        self.update.progress.admitted_at_epoch_ms = Some(now());
        self.update.progress.capacity_wait_ms = elapsed(self.queued);
        self.update.capacity = Some(capacity);
        self.send();
        self.update.capacity = None;
    }
    pub fn executing(&mut self) {
        self.update.progress.admitted = 0;
        self.update.progress.executing = 1;
        self.update.progress.started_at_epoch_ms = Some(now());
        self.started = Some(Instant::now());
        self.update.started_mono = self.started;
        self.send();
    }
    pub fn finish(&mut self, status: LeafStatus) {
        self.update.status = Some(status);
    }
    fn send(&self) {
        let _ = self.sender.send(self.update.clone());
    }
}
impl Drop for ProgressReporter {
    fn drop(&mut self) {
        if self.update.progress.admitted_at_epoch_ms.is_none() {
            self.update.progress.capacity_wait_ms = elapsed(self.queued);
        }
        self.update.progress.waiting = 0;
        self.update.progress.admitted = 0;
        self.update.progress.executing = 0;
        self.update.progress.finished = 1;
        self.update.finished_mono = Some(Instant::now());
        self.update.progress.finished_at_epoch_ms = Some(now());
        self.update.progress.process_duration_ms = self.started.map(elapsed).unwrap_or(0);
        self.update.status.get_or_insert(LeafStatus::Cancelled);
        self.send();
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
