use super::{CliValidationError, Invocation, invalid, remote};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub(super) enum StorageCommand {
    Inventory(InventoryArgs),
    Show {
        artifact_id: String,
    },
    Scan {
        #[arg(long)]
        repository_id: Option<String>,
        #[arg(long)]
        idempotency_key: String,
    },
    Register(FileArgs),
    LegacyRegister {
        #[arg(long)]
        deployment_id: String,
        #[arg(long)]
        expected_inventory_revision: u64,
        #[arg(long)]
        reason: String,
    },
    Protect {
        artifact_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        remove: bool,
    },
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    Roots {
        #[command(subcommand)]
        command: RootsCommand,
    },
    Cleanup {
        #[command(subcommand)]
        command: CleanupCommand,
    },
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
    History {
        #[arg(long)]
        artifact_id: Option<String>,
        #[arg(long)]
        before_ms: Option<u64>,
        #[arg(long)]
        limit: Option<u16>,
    },
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
}

#[derive(Debug, Args)]
pub(super) struct FileArgs {
    #[arg(long)]
    file: PathBuf,
}

#[derive(Debug, Default, Args)]
pub(super) struct InventoryArgs {
    #[arg(long)]
    repository_id: Option<String>,
    #[arg(long)]
    filesystem_id: Option<String>,
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    safety: Option<String>,
    #[arg(long)]
    query: Option<String>,
    #[arg(long)]
    include_removed: bool,
    #[arg(long, default_value_t = 0)]
    offset: u32,
    #[arg(long)]
    limit: Option<u16>,
}

#[derive(Debug, Subcommand)]
pub(super) enum PolicyCommand {
    Show {
        #[arg(long)]
        repository_id: Option<String>,
    },
    Set(FileArgs),
}

#[derive(Debug, Subcommand)]
pub(super) enum RootsCommand {
    List,
    Set(FileArgs),
}

#[derive(Debug, Subcommand)]
pub(super) enum CleanupCommand {
    Plan {
        #[arg(long, required = true)]
        artifact_id: Vec<String>,
        #[arg(long)]
        automatic: bool,
        #[arg(long)]
        include_persistent_data: bool,
    },
    Start {
        #[arg(long)]
        plan_id: String,
        #[arg(long)]
        idempotency_key: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum JobCommand {
    Status { job_id: String },
    Cancel { job_id: String },
}

#[derive(Debug, Subcommand)]
pub(super) enum LeaseCommand {
    Set(FileArgs),
    Release { lease_id: String },
}

fn read(args: FileArgs) -> Result<Value, CliValidationError> {
    let bytes =
        std::fs::read(args.file).map_err(|_| invalid("storage request file is unavailable"))?;
    if bytes.len() > devcoordinator2_api::MAX_REQUEST_BYTES {
        return Err(invalid("storage request is too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("storage request must be JSON"))
}

impl StorageCommand {
    pub(super) fn into_invocation(self) -> Result<Invocation, CliValidationError> {
        match self {
            Self::Inventory(a) => remote(
                "storage.inventory",
                json!({"repository_id":a.repository_id,"filesystem_id":a.filesystem_id,"kind":a.kind,"safety":a.safety,"query":a.query,"include_removed":a.include_removed,"offset":a.offset,"limit":a.limit}),
            ),
            Self::Show { artifact_id } => {
                remote("storage.artifact.get", json!({"artifact_id":artifact_id}))
            }
            Self::Scan {
                repository_id,
                idempotency_key,
            } => remote(
                "storage.scan",
                json!({"repository_id":repository_id,"idempotency_key":idempotency_key}),
            ),
            Self::Register(a) => remote("storage.register", read(a)?),
            Self::LegacyRegister {
                deployment_id,
                expected_inventory_revision,
                reason,
            } => remote(
                "storage.legacy.register",
                json!({"deployment_id":deployment_id,"expected_inventory_revision":expected_inventory_revision,"reason":reason}),
            ),
            Self::Protect {
                artifact_id,
                expected_revision,
                remove,
            } => remote(
                "storage.protection.set",
                json!({"artifact_id":artifact_id,"expected_revision":expected_revision,"protected":!remove}),
            ),
            Self::Policy {
                command: PolicyCommand::Show { repository_id },
            } => remote("storage.policy.get", json!({"repository_id":repository_id})),
            Self::Policy {
                command: PolicyCommand::Set(a),
            } => remote("storage.policy.set", read(a)?),
            Self::Roots {
                command: RootsCommand::List,
            } => remote("storage.roots.list", json!({})),
            Self::Roots {
                command: RootsCommand::Set(a),
            } => remote("storage.roots.set", read(a)?),
            Self::Cleanup {
                command:
                    CleanupCommand::Plan {
                        artifact_id,
                        automatic,
                        include_persistent_data,
                    },
            } => remote(
                "storage.cleanup.plan",
                json!({"artifact_ids":artifact_id,"automatic":automatic,"include_persistent_data":include_persistent_data}),
            ),
            Self::Cleanup {
                command:
                    CleanupCommand::Start {
                        plan_id,
                        idempotency_key,
                    },
            } => remote(
                "storage.cleanup.start",
                json!({"plan_id":plan_id,"idempotency_key":idempotency_key}),
            ),
            Self::Job {
                command: JobCommand::Status { job_id },
            } => remote("storage.job.status", json!({"job_id":job_id})),
            Self::Job {
                command: JobCommand::Cancel { job_id },
            } => remote("storage.job.cancel", json!({"job_id":job_id})),
            Self::History {
                artifact_id,
                before_ms,
                limit,
            } => remote(
                "storage.history",
                json!({"artifact_id":artifact_id,"before_ms":before_ms,"limit":limit}),
            ),
            Self::Lease {
                command: LeaseCommand::Set(a),
            } => remote("storage.lease.set", read(a)?),
            Self::Lease {
                command: LeaseCommand::Release { lease_id },
            } => remote("storage.lease.release", json!({"lease_id":lease_id})),
        }
    }
}
