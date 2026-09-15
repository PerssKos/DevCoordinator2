//! Execution implementation for validated schema-2 plans.
//!
//! Process execution is added independently from the protocol crate so the
//! eventual Rust daemon can link the engine without depending on the CLI.

mod capacity;
pub mod diagnostics;
mod error;
mod evidence;
pub mod log_query;
mod log_store;
mod process;
mod retention;
mod reuse;
mod runner;

pub use capacity::{
    CapacityObservation, LocalPermitProvider, PermitFuture, PermitProvider, PermitRequest,
    ReservationFuture, ResourceReservation, UnixPermitProvider,
};
pub use devcoordinator2_executor_protocol as protocol;
pub use error::ExecutorError;
pub use evidence::{artifact_receipts, receipts_match, source_digest};
pub use log_store::{
    CompleteLogWriter, IncompleteOperation, LeafLogMetadata, LeafSelector, LogStoreError,
    RunLogLease, RunLogMetadata, StreamMetadata,
};
pub use process::Cancellation;
pub use retention::{
    DEFAULT_HISTORY_DEPTH, DEFAULT_MAX_AGE_SECONDS, RetentionDecision, RetentionEntry,
    RetentionPolicy, select_expired,
};
pub use runner::Executor;

/// Stable private environment filename shared by the daemon and case executor.
pub fn case_environment_key(check: &str, case: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update((check.len() as u64).to_be_bytes());
    hash.update(check.as_bytes());
    hash.update(case.as_bytes());
    let digest = hash
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("case-{digest}")
}

mod progress;
