//! Coordinator-owned lifecycle for temporary governed-test containers.
//!
//! Agents invoke this service over the protocol; only the daemon's Docker
//! adapter ever receives a Docker command.  Ownership is checked against the
//! authenticated worktree, the persisted run/check record, and the exact
//! managed labels before every mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::database::OptionalExtension;
use devcoordinator2_api::ErrorCode;
use devcoordinator2_api::test_containers::{
    ContainerCleanupAttempt, ContainerCleanupStatus, ContainerCleanupSummary, ContainerCreate,
    ContainerDiagnostic, ContainerInspection, ContainerOperationResult, ContainerReference,
    TestContainerRecord,
};
use devcoordinator2_api::{ProtocolError, results::TestStatus};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{format_description::FormatItem, macros::format_description};

use crate::access::Caller;
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::docker::{
    ContainerLogCapture, CreateContainerRequest, DockerControl, DockerError, ExactContainerId,
    ManagedLabelContext, RunDetachedRequest,
};
use crate::platform::Clock;
use crate::repository::resolve_worktree;
use crate::test_state::CONTAINER_DIAGNOSTIC_CAP;
use crate::test_state::{ContainerLeaseAccess, ContainerMutationLease, TestRunStore};

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
const RECORD_CAP: usize = 256;
const MAX_LABEL_BYTES: usize = 256;
const CONTAINER_DIAGNOSTIC_TAIL_LINES: u16 = 500;
pub(crate) const FIXTURE_PHASE_TIMEOUT_SECONDS: u64 = 120;

#[derive(Clone)]
pub struct TestContainerService {
    database: Database,
    instance: String,
    docker: Arc<dyn DockerControl>,
    clock: Arc<dyn Clock>,
    store: TestRunStore,
}

/// Ephemeral protection derived from canonical receipts. It never grants
/// mutation authority or creates another ownership registry.
#[derive(Default)]
pub(crate) struct ContainerProtection {
    ids: BTreeSet<String>,
    intents: Vec<UnboundContainer>,
    scoped: bool,
}

struct UnboundContainer {
    name: String,
    owner: String,
    run: String,
    worktree: String,
}

impl ContainerProtection {
    fn retain(&mut self, records: &[TestContainerRecord]) {
        for record in records {
            if record.container_id == "0".repeat(64) {
                if record.cleanup_status != ContainerCleanupStatus::Complete {
                    self.intents.push(UnboundContainer {
                        name: record.name.clone(),
                        owner: record.owner_token.clone(),
                        run: record.run_id.clone(),
                        worktree: record.worktree_id.clone(),
                    });
                }
            } else {
                self.ids.insert(record.container_id.clone());
            }
        }
    }
}

enum MutationAccess {
    Ready(ContainerMutationLease),
    FixtureActive,
    InProgress,
}

enum CleanupOutcome {
    Settled,
    FixtureActive,
    InProgress,
}

enum CleanupFailure {
    Docker(DockerError),
    State(ProtocolError),
}

impl From<DockerError> for CleanupFailure {
    fn from(error: DockerError) -> Self {
        Self::Docker(error)
    }
}

/// A fixture's live lease and durable ownership reference. Dropping a handle
/// attempts cleanup; only a persisted absence receipt completes ownership.
pub(crate) struct OwnedTestContainer {
    service: TestContainerService,
    reference: ContainerReference,
    caller: Caller,
    lease: Option<File>,
    id: ExactContainerId,
    cleanup_attempted: bool,
}

impl OwnedTestContainer {
    pub fn id(&self) -> &ExactContainerId {
        &self.id
    }

    pub fn remove(&mut self) -> Result<(), String> {
        self.cleanup_attempted = true;
        self.lease.take();
        let deadline = fixture_cleanup_deadline();
        let _docker_deadline = crate::docker::DockerDeadlineScope::enter(deadline);
        let mut backoff = Duration::from_millis(10);
        loop {
            match self.service.remove_with_deadline(
                self.reference.clone(),
                &self.caller,
                "removed",
                deadline,
            ) {
                Ok(result) if result.record.cleanup_status == ContainerCleanupStatus::Complete => {
                    return Ok(());
                }
                Ok(_) => return Err(owned_cleanup_incomplete()),
                Err(error) if retryable_state(&error, "container_operation_in_progress") => {
                    return self.service.observe_owned_cleanup(
                        &self.reference,
                        &self.caller,
                        deadline,
                    );
                }
                Err(error) if retryable_state(&error, "container_metadata_busy") => {
                    if !wait_cleanup_budget(deadline, &mut backoff) {
                        return Err(owned_cleanup_pending());
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "owned database cleanup could not complete: {}",
                        error.message
                    ));
                }
            }
        }
    }
}

impl Drop for OwnedTestContainer {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _ = self.remove();
        }
    }
}

impl TestContainerService {
    pub fn new(
        config: &Config,
        database: Database,
        docker: Arc<dyn DockerControl>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            database,
            instance: config.unit_prefix.clone(),
            docker,
            clock,
            store: TestRunStore,
        }
    }

    pub(crate) fn create_fixture(
        &self,
        path: &Path,
        gid: u32,
        mut request: RunDetachedRequest,
    ) -> Result<OwnedTestContainer, String> {
        let run_id = request
            .label_context
            .run_id
            .clone()
            .ok_or("fixture run identity missing")?;
        let check = request
            .label_context
            .check
            .clone()
            .or_else(|| request.label_context.component.clone())
            .ok_or("fixture check identity missing")?;
        request.label_context.check = Some(check.clone());
        let caller = Caller {
            via_edge: false,
            pid: 0,
            uid: request.label_context.caller_uid,
            gid,
            client_kind: devcoordinator2_api::ClientKind::Other,
            model: None,
            effort: None,
            client_session: None,
            work: None,
            identity: None,
        };
        let context = self
            .resolve_owned(
                &path.to_string_lossy(),
                &run_id,
                &check,
                &caller,
                true,
                None,
            )
            .map_err(|_| "cannot retain fixture ownership")?;
        if context.status != TestStatus::Running
            || context.repository_id != request.label_context.repository_id
            || context.worktree_id != request.label_context.worktree_id
            || self.instance != request.label_context.instance
        {
            return Err("fixture ownership does not match its active run".into());
        }
        let owner = crate::ids::bug_id().map_err(|_| "cannot allocate fixture owner")?;
        request.label_context.owner = Some(owner.clone());
        request.retain_on_failure = true;
        let labels = crate::docker::managed_labels(&request.label_context, &BTreeMap::new())
            .map_err(|_| "cannot bind fixture ownership labels")?;
        let mut records = self
            .records(&context.records)
            .map_err(|_| "cannot read fixture ownership")?;
        if records.len() >= RECORD_CAP {
            return Err("temporary-container ownership record limit reached".into());
        }
        let mut record = pending_record(
            &context,
            request.name.clone(),
            request.image.clone(),
            request.command.clone(),
            owner.clone(),
            labels,
            self.timestamp()
                .map_err(|_| "cannot timestamp fixture ownership")?,
        );
        let lease = self
            .store
            .container_lease(&context.records, &owner, true)
            .map_err(|_| "cannot retain fixture lease")?;
        records.push(record.clone());
        self.write_records(&context.records, &records, context.uid, context.gid)
            .map_err(|_| "cannot retain fixture creation intent")?;
        context
            .unlock()
            .map_err(|_| "cannot release fixture receipt transaction")?;
        let result = self.docker.run_detached(&request);
        match &result {
            Ok(id) => {
                record.container_id = id.to_string();
                record.native_execution_began = true;
            }
            Err(error) => {
                record.operation_failed = true;
                let (exit_code, signal, _) = error_details(error);
                let detail = format!(
                    "Docker fixture creation {:?}; private command output withheld",
                    error.kind()
                );
                record.cleanup_attempts.push(ContainerCleanupAttempt {
                    attempt: 0,
                    attempted_at: record.created_at.clone(),
                    status: ContainerCleanupStatus::Failed,
                    stderr_bytes: detail.len() as u64,
                    stderr: Some(detail),
                    exit_code,
                    signal,
                    retryable: docker_retryable(error),
                });
            }
        }
        let retained = self.save_record(&context, &record);
        drop(context);
        match (result, retained) {
            (Ok(id), Ok(())) => Ok(OwnedTestContainer {
                service: self.clone(),
                reference: ContainerReference {
                    path: path.to_string_lossy().into_owned(),
                    run_id,
                    check,
                    container_id: id.to_string(),
                },
                caller,
                lease,
                id,
                cleanup_attempted: false,
            }),
            (result, _) => {
                drop(lease);
                let _ = self.cleanup_run(&path.to_string_lossy(), &run_id, &caller);
                match result {
                    Err(error) => Err(format!(
                        "cannot start owned database fixture (kind={:?}); inspect retained container ownership",
                        error.kind()
                    )),
                    Ok(_) => Err(
                        "cannot retain fixture identity; inspect retained container ownership"
                            .into(),
                    ),
                }
            }
        }
    }

    /// Reconcile a single owned record. An active lease prevents retries from
    /// deleting a shared template; crashes release that lease automatically.
    fn cleanup_record(
        &self,
        context: &Context,
        record: &mut TestContainerRecord,
        deadline: Instant,
    ) -> Result<CleanupOutcome, ProtocolError> {
        context.unlock()?;
        self.verify_record(record, context)?;
        self.refresh_record(context, record)?;
        if record.cleanup_status == ContainerCleanupStatus::Complete {
            return Ok(CleanupOutcome::Settled);
        }
        let _lease = match self.mutation_access(context, record)? {
            MutationAccess::Ready(lease) => lease,
            access => {
                self.refresh_record(context, record)?;
                return Ok(
                    if record.cleanup_status == ContainerCleanupStatus::Complete {
                        CleanupOutcome::Settled
                    } else if matches!(access, MutationAccess::FixtureActive) {
                        CleanupOutcome::FixtureActive
                    } else {
                        CleanupOutcome::InProgress
                    },
                );
            }
        };
        self.refresh_record(context, record)?;
        if record.cleanup_status == ContainerCleanupStatus::Complete {
            return Ok(CleanupOutcome::Settled);
        }
        let removal: Result<crate::docker::ContainerRemovalReceipt, CleanupFailure> = (|| {
            if record.container_id == "0".repeat(64) {
                let candidates = self.docker.list_ids_by_labels(&record.labels)?;
                let mut owned = Vec::new();
                for id in candidates {
                    self.inspect_owned(&id, &record.labels, &record.name).map_err(|error| {
                        if error.recovery.as_ref().is_some_and(|recovery| recovery.retryable) {
                            DockerError::Command("owned creation intent candidate inspection is unavailable".into())
                        } else {
                            DockerError::InvalidTarget("owned creation intent candidate failed exact name or ownership inspection".into())
                        }
                    })?;
                    owned.push(id);
                }
                if owned.len() > 1 {
                    return Err(DockerError::InvalidTarget(
                        "fixture intent resolved to multiple exact containers".into(),
                    )
                    .into());
                }
                if let Some(id) = owned.pop() {
                    record.container_id = id.to_string();
                    // Bind the recovered exact ID before any Docker mutation.
                    self.save_cleanup_record(context, record, deadline)
                        .map_err(CleanupFailure::State)?;
                } else if !self.docker.container_name_present(&record.name)? {
                    return Ok(crate::docker::ContainerRemovalReceipt {
                        command_error: None,
                    });
                } else {
                    return Err(DockerError::InvalidTarget(
                        "fixture name has no matching owned identity".into(),
                    )
                    .into());
                }
            }
            let id = exact_id(&record.container_id).map_err(|_| {
                DockerError::InvalidTarget("invalid persisted container identity".into())
            })?;
            if self.docker.container_present(&id)? {
                let inspection = self
                    .inspect_owned(&id, &record.labels, &record.name)
                    .map_err(|_| {
                        DockerError::InvalidTarget("container ownership inspection failed".into())
                    })?;
                record.solver_exit_code = inspection.exit_code;
                record.solver_signal = inspection.signal;
                record.oom_killed = inspection.oom_killed;
                if record.diagnostic.is_none() && unexpected_fixture_exit(&inspection) {
                    if let Ok(capture) = self
                        .docker
                        .container_log_capture(&id, CONTAINER_DIAGNOSTIC_TAIL_LINES)
                    {
                        record.diagnostic = Some(
                            self.retain_diagnostic(context, record, &capture)
                                .map_err(CleanupFailure::State)?,
                        );
                    }
                }
                // Persist exit/OOM state and the diagnostic reference while the
                // exact container still exists. Cleanup may then safely remove
                // it without losing the failure boundary.
                self.save_cleanup_record(context, record, deadline)
                    .map_err(CleanupFailure::State)?;
            }
            if Instant::now() >= deadline {
                return Err(CleanupFailure::State(cleanup_receipt_pending_error(record)));
            }
            self.docker
                .remove_container_receipt(&id, &record.name, true)
                .map_err(CleanupFailure::Docker)
        })();
        let (status, retryable, diagnostics) = match removal {
            Ok(receipt) => (
                ContainerCleanupStatus::Complete,
                false,
                receipt
                    .command_error
                    .map(|error| cleanup_details(record, &error)),
            ),
            Err(CleanupFailure::State(error)) => return Err(error),
            Err(CleanupFailure::Docker(error)) => {
                let retryable = docker_retryable(&error);
                (
                    if retryable {
                        ContainerCleanupStatus::Pending
                    } else {
                        ContainerCleanupStatus::Failed
                    },
                    retryable,
                    Some(cleanup_details(record, &error)),
                )
            }
        };
        record.cleanup_status = status;
        record.cleanup_attempts.push(ContainerCleanupAttempt {
            attempt: u32::try_from(record.cleanup_attempts.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1),
            attempted_at: self.timestamp()?,
            status,
            stderr: diagnostics.as_ref().map(|(_, _, text)| text.clone()),
            stderr_bytes: diagnostics
                .as_ref()
                .map_or(0, |(_, _, text)| text.len() as u64),
            exit_code: diagnostics.as_ref().and_then(|(code, _, _)| *code),
            signal: diagnostics.as_ref().and_then(|(_, signal, _)| *signal),
            retryable,
        });
        self.save_cleanup_record(context, record, deadline)?;
        Ok(CleanupOutcome::Settled)
    }

    pub fn create(
        &self,
        params: ContainerCreate,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        validate_identity(&params.run_id, "run_id", 96)?;
        validate_identity(&params.check, "check", 96)?;
        validate_image(&params.image)?;
        validate_command(&params.command)?;
        let context = self.resolve_owned(
            &params.path,
            &params.run_id,
            &params.check,
            caller,
            true,
            None,
        )?;
        if context.status != TestStatus::Running {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "temporary containers can only be created for a running governed test",
            ));
        }
        let mut records = self.records(&context.records)?;
        if records.iter().any(|record| {
            record.check == params.check
                && record.cleanup_status != ContainerCleanupStatus::Complete
        }) {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "the governed check already owns a live temporary container",
            ));
        }
        if records.len() >= RECORD_CAP {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "temporary-container ownership record limit reached",
            ));
        }
        let owner_token = crate::ids::bug_id().map_err(|_| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "cannot allocate container owner token",
            )
        })?;
        let name = generated_name(&params.run_id, &params.check, &owner_token)?;
        let created_at = self.timestamp()?;
        let label_context = ManagedLabelContext {
            instance: self.instance.clone(),
            repository_id: context.repository_id.clone(),
            worktree_id: context.worktree_id.clone(),
            run_id: Some(params.run_id.clone()),
            check: Some(params.check.clone()),
            owner: Some(owner_token.clone()),
            deployment_id: None,
            component: None,
            generation: None,
            ttl_seconds: None,
            purpose: "test".into(),
            caller_uid: context.execution_uid,
            client: client_name(caller),
            session: caller.client_session.clone(),
            created_at: created_at.clone(),
            data_class: "disposable".into(),
        };
        let labels = crate::docker::managed_labels(&label_context, &BTreeMap::new())
            .map_err(|error| docker_error("cannot create governed container labels", error))?;
        let mut record = pending_record(
            &context,
            name.clone(),
            params.image.clone(),
            params.command.clone(),
            owner_token,
            labels,
            created_at,
        );
        records.push(record.clone());
        self.write_records(&context.records, &records, context.uid, context.gid)?;
        let _lease = self
            .store
            .container_lease(&context.records, &record.owner_token, true)
            .map_err(state_error)?;
        context.unlock()?;
        let id = match self.docker.create_container(&CreateContainerRequest {
            name: name.clone(),
            image: params.image,
            label_context,
            labels: BTreeMap::new(),
            env_file: None,
            publish: Vec::new(),
            volumes: Vec::new(),
            command: params.command,
            restart: "no".into(),
        }) {
            Ok(id) => id,
            Err(error) => {
                record.operation_failed = true;
                self.save_record(&context, &record)?;
                drop(_lease);
                self.cleanup_record(&context, &mut record, fixture_cleanup_deadline())?;
                return Err(docker_error("cannot create governed container", error));
            }
        };
        record.container_id = id.to_string();
        self.save_record(&context, &record)?;
        let inspection = match self.inspect_owned(&id, &record.labels, &name) {
            Ok(value) => value,
            Err(error) => {
                record.operation_failed = true;
                self.save_record(&context, &record)?;
                drop(_lease);
                self.cleanup_record(&context, &mut record, fixture_cleanup_deadline())?;
                return Err(error);
            }
        };
        Ok(ContainerOperationResult {
            operation: "created".into(),
            record,
            inspection: Some(inspection),
        })
    }

    pub fn start(
        &self,
        params: ContainerReference,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        let context = self.resolve_reference(&params, caller)?;
        if context.status != TestStatus::Running {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "temporary containers can only start for the current running test",
            ));
        }
        let (records, index) = self.find_record(&context, &params)?;
        let mut record = records[index].clone();
        self.verify_record(&record, &context)?;
        context.unlock()?;
        self.refresh_record(&context, &mut record)?;
        self.verify_record(&record, &context)?;
        if record.cleanup_status == ContainerCleanupStatus::Complete {
            return Err(completed_start_error());
        }
        let _lease = match self.mutation_access(&context, &record)? {
            MutationAccess::Ready(lease) => lease,
            access => {
                self.refresh_record(&context, &mut record)?;
                return Err(
                    if record.cleanup_status == ContainerCleanupStatus::Complete {
                        completed_start_error()
                    } else if matches!(access, MutationAccess::FixtureActive) {
                        fixture_active_error()
                    } else {
                        operation_busy_error(&record)
                    },
                );
            }
        };
        self.refresh_record(&context, &mut record)?;
        if record.cleanup_status == ContainerCleanupStatus::Complete {
            return Err(completed_start_error());
        }
        let id = exact_id(&record.container_id)?;
        if id.as_str() != params.container_id {
            return Err(ownership_error());
        }
        self.require_creation_open(&context.worktree, &context.run_id)?;
        if let Err(error) = self.inspect_owned(&id, &record.labels, &record.name) {
            record.operation_failed = true;
            record.cleanup_status = ContainerCleanupStatus::Failed;
            self.save_record(&context, &record)?;
            return Err(error);
        }
        if let Err(error) = self.docker.start_container(&id) {
            let (exit_code, signal, stderr) = error_details(&error);
            record.operation_failed = true;
            record.cleanup_attempts.push(ContainerCleanupAttempt {
                attempt: 0,
                attempted_at: self.timestamp()?,
                status: ContainerCleanupStatus::Failed,
                stderr_bytes: stderr.len() as u64,
                stderr: Some(stderr),
                exit_code,
                signal,
                retryable: false,
            });
            self.save_record(&context, &record)?;
            return Err(docker_error("cannot start governed container", error));
        }
        record.native_execution_began = true;
        self.save_record(&context, &record)?;
        let inspection = self.inspect_owned(&id, &record.labels, &record.name)?;
        record.solver_exit_code = inspection.exit_code;
        record.solver_signal = inspection.signal;
        self.save_record(&context, &record)?;
        Ok(ContainerOperationResult {
            operation: "started".into(),
            record,
            inspection: Some(inspection),
        })
    }

    pub fn inspect(
        &self,
        params: ContainerReference,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        let context = self.resolve_reference(&params, caller)?;
        let (mut records, index) = self.find_record(&context, &params)?;
        let id = exact_id(&params.container_id)?;
        if records[index].container_id != id.as_str() {
            return Err(ownership_error());
        }
        self.verify_record(&records[index], &context)?;
        context.unlock()?;
        let inspection = if records[index].cleanup_status == ContainerCleanupStatus::Complete {
            None
        } else {
            match self.inspect_owned(&id, &records[index].labels, &records[index].name) {
                Ok(inspection) => {
                    records[index].solver_exit_code = inspection.exit_code;
                    records[index].solver_signal = inspection.signal;
                    records[index].oom_killed = inspection.oom_killed;
                    let mut inspection = inspection;
                    inspection.diagnostic = records[index].diagnostic.clone();
                    Some(inspection)
                }
                Err(error) => {
                    if self.docker.container_present(&id).map_err(|failure| {
                        docker_error("cannot observe governed container", failure)
                    })? {
                        return Err(error);
                    }
                    None
                }
            }
        };
        self.archive_receipt(&context)?;
        Ok(ContainerOperationResult {
            operation: "inspected".into(),
            record: records[index].clone(),
            inspection,
        })
    }

    pub fn remove(
        &self,
        params: ContainerReference,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        self.remove_or_retry(params, caller, "removed")
    }

    pub fn retry(
        &self,
        params: ContainerReference,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        self.remove_or_retry(params, caller, "cleanup_retried")
    }

    /// Retry cleanup for every persisted temporary container owned by one run.
    /// Test finalization uses this method instead of issuing Docker commands so
    /// an individual cleanup failure remains visible in the terminal summary.
    pub fn cleanup_run(
        &self,
        path: &str,
        run_id: &str,
        caller: &Caller,
    ) -> Result<ContainerCleanupSummary, ProtocolError> {
        validate_identity(run_id, "run_id", 96)?;
        let initial = self.resolve(path, run_id, "cleanup", caller);
        let context = match initial {
            Ok(context) => context,
            Err(error) if error.code == ErrorCode::TestNotFound => {
                return Ok(ContainerCleanupSummary {
                    run_id: run_id.into(),
                    cleanup_status: ContainerCleanupStatus::Complete,
                    native_execution_started: false,
                    cleanup_completed: true,
                    pending_count: 0,
                    failed_count: 0,
                    failed_operations: 0,
                    records: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let mut records = self.records(&context.records)?;
        context.unlock()?;
        let mut leased_templates = Vec::new();
        for record in records.iter_mut().filter(|record| {
            record.run_id == run_id
                && record.repository_id == context.repository_id
                && record.worktree_id == context.worktree_id
        }) {
            if record.cleanup_status == ContainerCleanupStatus::Complete {
                continue;
            }
            if matches!(
                self.cleanup_record(&context, record, fixture_cleanup_deadline())?,
                CleanupOutcome::FixtureActive
            ) && record
                .labels
                .get("devcoordinator2.component")
                .is_some_and(|component| component == "database-template")
            {
                leased_templates.push(record.owner_token.clone());
            }
        }
        records = self.current_records(&context)?;
        self.archive_receipt(&context)?;
        // Warm templates retain their own live lease beyond the creating run.
        // They are not that run's cleanup obligation until the pool retires them.
        records.retain(|record| !leased_templates.contains(&record.owner_token));
        let native_execution_started = records.iter().any(|record| {
            record.run_id == run_id
                && record.repository_id == context.repository_id
                && record.worktree_id == context.worktree_id
                && record.native_execution_began
        });
        let pending_count = records
            .iter()
            .filter(|record| {
                record.run_id == run_id
                    && record.repository_id == context.repository_id
                    && record.worktree_id == context.worktree_id
                    && record.cleanup_status == ContainerCleanupStatus::Pending
            })
            .count() as u32;
        let failed_count = records
            .iter()
            .filter(|record| {
                record.run_id == run_id
                    && record.repository_id == context.repository_id
                    && record.worktree_id == context.worktree_id
                    && record.cleanup_status == ContainerCleanupStatus::Failed
            })
            .count() as u32;
        let failed_operations = records
            .iter()
            .filter(|record| {
                record.run_id == run_id
                    && record.repository_id == context.repository_id
                    && record.worktree_id == context.worktree_id
                    && record.operation_failed
            })
            .count() as u32;
        let cleanup_status = if failed_count > 0 {
            ContainerCleanupStatus::Failed
        } else if pending_count > 0 {
            ContainerCleanupStatus::Pending
        } else {
            ContainerCleanupStatus::Complete
        };
        Ok(ContainerCleanupSummary {
            run_id: run_id.into(),
            cleanup_status,
            native_execution_started,
            cleanup_completed: cleanup_status == ContainerCleanupStatus::Complete,
            pending_count,
            failed_count,
            failed_operations,
            records: records
                .into_iter()
                .filter(|record| {
                    record.run_id == run_id
                        && record.repository_id == context.repository_id
                        && record.worktree_id == context.worktree_id
                })
                .collect(),
        })
    }

    fn remove_or_retry(
        &self,
        params: ContainerReference,
        caller: &Caller,
        operation: &str,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        self.remove_with_deadline(params, caller, operation, fixture_cleanup_deadline())
    }

    fn remove_with_deadline(
        &self,
        params: ContainerReference,
        caller: &Caller,
        operation: &str,
        deadline: Instant,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        let context = self.resolve_reference(&params, caller)?;
        let (mut records, index) = self.find_record(&context, &params)?;
        let id = exact_id(&params.container_id)?;
        if records[index].container_id != id.as_str() {
            return Err(ownership_error());
        }
        self.verify_record(&records[index], &context)?;
        context.unlock()?;
        if records[index].cleanup_status == ContainerCleanupStatus::Complete {
            self.archive_receipt(&context)?;
            return Ok(ContainerOperationResult {
                operation: operation.into(),
                record: records[index].clone(),
                inspection: None,
            });
        }
        match self.cleanup_record(&context, &mut records[index], deadline)? {
            CleanupOutcome::FixtureActive => return Err(fixture_active_error()),
            CleanupOutcome::InProgress => return Err(operation_busy_error(&records[index])),
            CleanupOutcome::Settled => {}
        }
        let status = records[index].cleanup_status;
        if status == ContainerCleanupStatus::Complete {
            self.archive_receipt(&context)?;
        }
        let inspection = if status == ContainerCleanupStatus::Complete {
            None
        } else {
            self.docker
                .inspect(&id)
                .ok()
                .and_then(|value| self.to_inspection(&value, &records[index]))
        };
        Ok(ContainerOperationResult {
            operation: operation.into(),
            record: records[index].clone(),
            inspection,
        })
    }

    fn resolve(
        &self,
        path: &str,
        run_id: &str,
        check: &str,
        caller: &Caller,
    ) -> Result<Context, ProtocolError> {
        self.resolve_owned(path, run_id, check, caller, false, None)
    }

    fn resolve_reference(
        &self,
        params: &ContainerReference,
        caller: &Caller,
    ) -> Result<Context, ProtocolError> {
        self.resolve_owned(
            &params.path,
            &params.run_id,
            &params.check,
            caller,
            false,
            Some(&params.container_id),
        )
    }

    fn resolve_owned(
        &self,
        path: &str,
        run_id: &str,
        check: &str,
        caller: &Caller,
        create: bool,
        candidate: Option<&str>,
    ) -> Result<Context, ProtocolError> {
        if path.is_empty() || !Path::new(path).is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be an absolute registered worktree path",
            ));
        }
        let (worktree, worktree_id, repository_id) = if caller.is_console() {
            let requested = path.to_owned();
            self.database
                .read(move |connection| {
                    connection
                        .query_row(
                            "SELECT worktree_path,worktree_id,repository_id FROM worktrees WHERE worktree_path=$1",
                            [&requested],
                            |row| {
                                Ok((
                                    PathBuf::from(row.get::<_, String>(0)?),
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(DatabaseError::from)
                })
                .map_err(database_error)?
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::RepositoryNotFound,
                        "no registered worktree matches path",
                    )
                })?
        } else {
            let info = resolve_worktree(Path::new(path), Some((caller.uid, caller.gid))).map_err(
                |_| {
                    ProtocolError::new(
                        ErrorCode::RepositoryNotFound,
                        "caller does not own a registered worktree at path",
                    )
                },
            )?;
            let worktree_id = crate::ids::worktree_id(&info.worktree_root).map_err(id_error)?;
            let repository_id =
                crate::ids::repository_id(&info.repository_root).map_err(id_error)?;
            (info.worktree_root.to_path_buf(), worktree_id, repository_id)
        };
        validate_identity(run_id, "run_id", 96)?;
        let summary = self
            .store
            .read_current_summary(&worktree)
            .map_err(state_error)?;
        let current = if summary
            .as_ref()
            .is_some_and(|summary| summary.run_id == run_id)
        {
            self.store.open_current(&worktree).map_err(state_error)?
        } else {
            None
        };
        let owner = current
            .as_ref()
            .map(|current| self.store.owner(current))
            .transpose()
            .map_err(state_error)?;
        if create {
            self.require_creation_open(&worktree, run_id)?;
        }
        let mut records = self
            .store
            .open_container_run(&worktree, run_id, None)
            .map_err(state_error)?;
        if records.is_none()
            && let Some(current) = current.as_ref()
        {
            let legacy_ids = self
                .store
                .read_legacy_containers(current)
                .map_err(state_error)?;
            let legacy = !self.records(current)?.is_empty()
                || candidate.is_some_and(|id| legacy_ids.iter().any(|value| value == id));
            if create || legacy {
                records = self
                    .store
                    .open_container_run(&worktree, run_id, owner)
                    .map_err(state_error)?;
            }
        }
        let records = records.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::TestNotFound,
                "retained container ownership is unavailable for this run",
            )
        })?;
        let (uid, gid) = self.store.owner(&records).map_err(state_error)?;
        // Migrate existing typed receipts once. Legacy untyped Docker labels
        // alone never grant ownership of an arbitrary container.
        if let Some(current) = current.as_ref() {
            let legacy = self.records(current)?;
            let mut durable = self.records(&records)?;
            let previous_count = durable.len();
            for record in legacy.into_iter().filter(|record| {
                record.run_id == run_id
                    && record.repository_id == repository_id
                    && record.worktree_id == worktree_id
            }) {
                if !durable
                    .iter()
                    .any(|value| value.owner_token == record.owner_token)
                {
                    durable.push(record);
                }
            }
            if durable.len() != previous_count {
                self.write_records(&records, &durable, uid, gid)?;
            }
            self.store
                .retire_legacy_container_receipts(current, &durable, uid, gid)
                .map_err(state_error)?;
        }
        if create {
            self.require_creation_open(&worktree, run_id)?;
        }
        let status = summary
            .filter(|summary| summary.run_id == run_id)
            .map_or(TestStatus::Interrupted, |summary| summary.status);
        Ok(Context {
            worktree,
            worktree_id,
            repository_id,
            records,
            status,
            execution_uid: uid,
            uid,
            gid,
            run_id: run_id.into(),
            check: check.into(),
        })
    }

    fn refresh_record(
        &self,
        context: &Context,
        record: &mut TestContainerRecord,
    ) -> Result<(), ProtocolError> {
        *record = self
            .current_records(context)?
            .into_iter()
            .find(|value| value.owner_token == record.owner_token)
            .ok_or_else(ownership_error)?;
        self.verify_record_labels(record)
    }

    fn mutation_access(
        &self,
        context: &Context,
        record: &TestContainerRecord,
    ) -> Result<MutationAccess, ProtocolError> {
        Ok(
            match self
                .store
                .container_mutation_lease(&context.records, &record.owner_token)
                .map_err(state_error)?
            {
                ContainerLeaseAccess::Ready(lease) => MutationAccess::Ready(lease),
                ContainerLeaseAccess::ActiveUse
                    if record.labels.contains_key("devcoordinator2.component") =>
                {
                    MutationAccess::FixtureActive
                }
                ContainerLeaseAccess::ActiveUse | ContainerLeaseAccess::OperationInProgress => {
                    MutationAccess::InProgress
                }
            },
        )
    }

    fn observe_owned_cleanup(
        &self,
        reference: &ContainerReference,
        caller: &Caller,
        deadline: Instant,
    ) -> Result<(), String> {
        let mut backoff = Duration::from_millis(10);
        loop {
            let observation = (|| -> Result<Option<bool>, ProtocolError> {
                let context = self.resolve_reference(reference, caller)?;
                let (records, index) = self.find_record(&context, reference)?;
                let mut record = records[index].clone();
                self.verify_record(&record, &context)?;
                context.unlock()?;
                if record.cleanup_status == ContainerCleanupStatus::Complete {
                    return Ok(Some(true));
                }
                let Some(_operation) = self
                    .store
                    .container_operation_lease(&context.records, &record.owner_token)
                    .map_err(state_error)?
                else {
                    return Ok(None);
                };
                self.refresh_record(&context, &mut record)?;
                Ok(Some(
                    record.cleanup_status == ContainerCleanupStatus::Complete,
                ))
            })();
            match observation {
                Ok(Some(true)) => return Ok(()),
                Ok(Some(false)) => return Err(owned_cleanup_incomplete()),
                Ok(None) => {}
                Err(error) if retryable_state(&error, "container_metadata_busy") => {}
                Err(error) => {
                    return Err(format!(
                        "owned database cleanup could not be observed: {}",
                        error.message
                    ));
                }
            }
            if !wait_cleanup_budget(deadline, &mut backoff) {
                return Err(owned_cleanup_pending());
            }
        }
    }

    fn save_cleanup_record(
        &self,
        context: &Context,
        record: &TestContainerRecord,
        deadline: Instant,
    ) -> Result<(), ProtocolError> {
        let mut backoff = Duration::from_millis(10);
        loop {
            match self.save_record(context, record) {
                Ok(()) => return Ok(()),
                Err(error) if retryable_state(&error, "container_metadata_busy") => {
                    if !wait_cleanup_budget(deadline, &mut backoff) {
                        return Err(cleanup_receipt_pending_error(record));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn require_creation_open(&self, worktree: &Path, run_id: &str) -> Result<(), ProtocolError> {
        if self
            .store
            .container_creation_open(worktree, run_id)
            .map_err(state_error)?
        {
            Ok(())
        } else {
            Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "governed test no longer accepts container creation or starts",
            ))
        }
    }

    fn current_records(
        &self,
        context: &Context,
    ) -> Result<Vec<TestContainerRecord>, ProtocolError> {
        let directory = self
            .store
            .open_container_run(&context.worktree, &context.run_id, None)
            .map_err(state_error)?
            .ok_or_else(ownership_error)?;
        self.records(&directory)
    }

    fn save_record(
        &self,
        context: &Context,
        record: &TestContainerRecord,
    ) -> Result<(), ProtocolError> {
        let directory = self
            .store
            .open_container_run(&context.worktree, &context.run_id, None)
            .map_err(state_error)?
            .ok_or_else(ownership_error)?;
        let mut records = self.records(&directory)?;
        let stored = records
            .iter_mut()
            .find(|value| value.owner_token == record.owner_token)
            .ok_or_else(ownership_error)?;
        if stored.labels != record.labels || stored.name != record.name {
            return Err(ownership_error());
        }
        *stored = record.clone();
        self.write_records(&directory, &records, context.uid, context.gid)
    }

    fn retain_diagnostic(
        &self,
        context: &Context,
        record: &TestContainerRecord,
        capture: &ContainerLogCapture,
    ) -> Result<ContainerDiagnostic, ProtocolError> {
        let payload = capture.text.as_bytes();
        if payload.len() > CONTAINER_DIAGNOSTIC_CAP {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "container diagnostics exceeded the bounded capture",
            ));
        }
        let directory = self
            .store
            .open_container_run(&context.worktree, &context.run_id, None)
            .map_err(state_error)?
            .ok_or_else(ownership_error)?;
        let reference = self
            .store
            .write_container_diagnostics(
                &directory,
                &record.owner_token,
                payload,
                context.uid,
                context.gid,
            )
            .map_err(state_error)?;
        Ok(ContainerDiagnostic {
            reference,
            bytes: payload.len() as u64,
            sha256: lower_hex(&Sha256::digest(payload)),
            truncated: capture.truncated,
        })
    }

    fn archive_receipt(&self, context: &Context) -> Result<(), ProtocolError> {
        if context.status != TestStatus::Running
            && let Some(directory) = self
                .store
                .open_container_run(&context.worktree, &context.run_id, None)
                .map_err(state_error)?
        {
            self.store
                .archive_container_run(&context.worktree, &context.run_id, &directory)
                .map_err(state_error)?;
        }
        Ok(())
    }

    pub(crate) fn has_pending_run(&self, worktree: &Path, run_id: &str) -> bool {
        self.store
            .container_run_pending(worktree, run_id)
            .unwrap_or(true)
    }

    pub(crate) fn protection_for_scope(
        &self,
        worktree: &Path,
        run: Option<&str>,
    ) -> Result<ContainerProtection, ProtocolError> {
        let runs = match run {
            Some(run) => vec![run.to_owned()],
            None => self.store.container_runs(worktree).map_err(state_error)?,
        };
        let mut protection = ContainerProtection {
            scoped: true,
            ..ContainerProtection::default()
        };
        for run in runs {
            if let Some(directory) = self
                .store
                .open_container_run(worktree, &run, None)
                .map_err(state_error)?
            {
                let records = self.records(&directory)?;
                if records.iter().any(|record| record.run_id != run) {
                    return Err(ownership_error());
                }
                protection.retain(&records);
            }
        }
        Ok(protection)
    }

    /// Legacy cleanup consults canonical scope authority, never a lookup
    /// selected by possibly damaged observed run/worktree labels.
    pub(crate) fn is_recorded_container(
        &self,
        id: &ExactContainerId,
        protection: &ContainerProtection,
    ) -> Result<bool, ProtocolError> {
        if protection.ids.contains(id.as_str())
            || (protection.scoped && !protection.intents.is_empty())
        {
            return Ok(true);
        }
        if protection.intents.is_empty() {
            return Ok(false);
        }
        let value = self
            .docker
            .inspect(id)
            .map_err(|error| docker_error("cannot classify exact cleanup ownership", error))?;
        let actual = self
            .to_inspection_value(&value, id)
            .map_err(|_| ownership_error())?;
        Ok(protection.intents.iter().any(|intent| {
            actual.name == intent.name
                || actual.labels.get("devcoordinator2.owner") == Some(&intent.owner)
                || actual.labels.get("devcoordinator2.run") == Some(&intent.run)
                || actual.labels.get("devcoordinator2.worktree") == Some(&intent.worktree)
        }))
    }

    fn records(&self, current: &std::fs::File) -> Result<Vec<TestContainerRecord>, ProtocolError> {
        self.store
            .read_container_records(current)
            .map_err(state_error)
    }

    fn write_records(
        &self,
        current: &std::fs::File,
        records: &[TestContainerRecord],
        uid: u32,
        gid: u32,
    ) -> Result<(), ProtocolError> {
        self.store
            .write_container_records(current, records, uid, gid)
            .map_err(state_error)
    }

    fn find_record(
        &self,
        context: &Context,
        params: &ContainerReference,
    ) -> Result<(Vec<TestContainerRecord>, usize), ProtocolError> {
        let mut records = self.records(&context.records)?;
        let index = records.iter().position(|record| {
            record.container_id == params.container_id
                && record.run_id == context.run_id
                && record.check == context.check
                && record.repository_id == context.repository_id
                && record.worktree_id == context.worktree_id
        });
        let index = if let Some(index) = index {
            index
        } else {
            self.adopt_legacy_fixture(context, params, &mut records)?
        };
        Ok((records, index))
    }

    /// Older shared fixtures have an exact ID receipt but no typed lifecycle
    /// record. Recover only that retained ID, its deterministic fixture name,
    /// and the complete run/repository/worktree/owner label binding.
    fn adopt_legacy_fixture(
        &self,
        context: &Context,
        params: &ContainerReference,
        records: &mut Vec<TestContainerRecord>,
    ) -> Result<usize, ProtocolError> {
        let summary = self
            .store
            .read_current_summary(&context.worktree)
            .map_err(state_error)?
            .filter(|summary| summary.run_id == context.run_id)
            .ok_or_else(ownership_error)?;
        if !context.check.starts_with("shared-") || records.len() >= RECORD_CAP {
            return Err(ownership_error());
        }
        let current = self
            .store
            .open_current(&context.worktree)
            .map_err(state_error)?
            .ok_or_else(ownership_error)?;
        if !self
            .store
            .read_legacy_containers(&current)
            .map_err(state_error)?
            .contains(&params.container_id)
        {
            return Err(ownership_error());
        }
        context.unlock()?;
        let id = exact_id(&params.container_id)?;
        let value = self
            .docker
            .inspect(&id)
            .map_err(|error| docker_error("cannot inspect retained fixture", error))?;
        let actual = self
            .to_inspection_value(&value, &id)
            .map_err(|_| ownership_error())?;
        let expected_name = format!(
            "devcoordinator2-test-{}-postgres-{}",
            context.run_id.trim_start_matches('t'),
            context.check
        );
        if actual.name != expected_name
            || actual.labels.get("devcoordinator2.component") != Some(&context.check)
        {
            return Err(ownership_error());
        }
        let owner = actual
            .labels
            .get("devcoordinator2.owner")
            .cloned()
            .ok_or_else(ownership_error)?;
        validate_identity(&owner, "owner", MAX_LABEL_BYTES)?;
        let labels = crate::docker::managed_labels(
            &ManagedLabelContext {
                instance: self.instance.clone(),
                repository_id: context.repository_id.clone(),
                worktree_id: context.worktree_id.clone(),
                run_id: Some(context.run_id.clone()),
                check: Some(context.check.clone()),
                owner: Some(owner.clone()),
                deployment_id: None,
                component: Some(context.check.clone()),
                generation: None,
                ttl_seconds: None,
                purpose: "test".into(),
                caller_uid: context.execution_uid,
                client: summary.client,
                session: actual.labels.get("devcoordinator2.session").cloned(),
                created_at: summary.started_at,
                data_class: "disposable".into(),
            },
            &BTreeMap::new(),
        )
        .map_err(|_| ownership_error())?;
        if !ownership_labels_match(&actual.labels, &labels) {
            return Err(ownership_error());
        }
        let mut record = pending_record(
            context,
            expected_name,
            actual.image,
            Vec::new(),
            owner,
            labels,
            self.timestamp()?,
        );
        record.container_id = id.to_string();
        record.native_execution_began = matches!(
            actual.state.as_str(),
            "running" | "exited" | "restarting" | "paused" | "dead"
        );
        self.verify_record(&record, context)?;
        let directory = self
            .store
            .open_container_run(&context.worktree, &context.run_id, None)
            .map_err(state_error)?
            .ok_or_else(ownership_error)?;
        *records = self.records(&directory)?;
        if let Some(index) = records.iter().position(|value| {
            value.container_id == record.container_id && value.check == record.check
        }) {
            return Ok(index);
        }
        if records.len() >= RECORD_CAP {
            return Err(ownership_error());
        }
        records.push(record);
        self.write_records(&directory, records, context.uid, context.gid)?;
        Ok(records.len() - 1)
    }

    pub(crate) fn recover_registered(&self) -> Result<ContainerProtection, ProtocolError> {
        let worktrees = self
            .database
            .read(|connection| {
                let mut statement = connection
                    .prepare("SELECT worktree_path FROM worktrees ORDER BY worktree_id")?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        let mut recorded = ContainerProtection::default();
        for path in worktrees {
            let worktree = Path::new(&path);
            let runs = match self.store.container_runs(worktree) {
                Ok(runs) => runs,
                Err(_) => continue,
            };
            for run_id in runs {
                let Some(directory) = self
                    .store
                    .open_container_run(worktree, &run_id, None)
                    .map_err(state_error)?
                else {
                    continue;
                };
                let (uid, gid) = self.store.owner(&directory).map_err(state_error)?;
                let records = self.records(&directory)?;
                recorded.retain(&records);
                drop(directory);
                for record in records
                    .iter()
                    .filter(|record| record.container_id == "0".repeat(64))
                {
                    if let Ok(candidates) = self.docker.list_ids_by_labels(&record.labels) {
                        recorded
                            .ids
                            .extend(candidates.into_iter().map(|id| id.to_string()));
                    }
                }
                let caller = Caller {
                    via_edge: false,
                    pid: 0,
                    uid,
                    gid,
                    client_kind: devcoordinator2_api::ClientKind::Other,
                    model: None,
                    effort: None,
                    client_session: None,
                    work: None,
                    identity: None,
                };
                // Failed cleanup remains a durable inspect/retry obligation.
                // It must not prevent unrelated repositories from recovering.
                let _ = self.cleanup_run(&path, &run_id, &caller);
            }
        }
        Ok(recorded)
    }

    fn verify_record(
        &self,
        record: &TestContainerRecord,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        if record.run_id != context.run_id
            || record.check != context.check
            || record.repository_id != context.repository_id
            || record.worktree_id != context.worktree_id
        {
            return Err(ownership_error());
        }
        self.verify_record_labels(record)?;
        if record
            .diagnostic
            .as_ref()
            .is_some_and(|diagnostic| !valid_diagnostic(diagnostic, &record.owner_token))
        {
            return Err(ownership_error());
        }
        Ok(())
    }

    fn verify_record_labels(&self, record: &TestContainerRecord) -> Result<(), ProtocolError> {
        if record.labels.get("devcoordinator2.instance") != Some(&self.instance)
            || record.labels.get("devcoordinator2.run") != Some(&record.run_id)
            || record.labels.get("devcoordinator2.check") != Some(&record.check)
            || record.labels.get("devcoordinator2.repository") != Some(&record.repository_id)
            || record.labels.get("devcoordinator2.worktree") != Some(&record.worktree_id)
            || record.labels.get("devcoordinator2.owner") != Some(&record.owner_token)
            || record.labels.get("devcoordinator2.purpose") != Some(&"test".to_owned())
            || record.labels.get("devcoordinator2.data") != Some(&"disposable".to_owned())
        {
            return Err(ownership_error());
        }
        Ok(())
    }

    fn inspect_owned(
        &self,
        id: &ExactContainerId,
        labels: &BTreeMap<String, String>,
        expected_name: &str,
    ) -> Result<ContainerInspection, ProtocolError> {
        let value = self
            .docker
            .inspect(id)
            .map_err(|error| docker_error("cannot inspect governed container", error))?;
        let mut actual = self
            .to_inspection_value(&value, id)
            .map_err(|message| ProtocolError::new(ErrorCode::TestStartFailed, message))?;
        if actual.name != expected_name || !ownership_labels_match(&actual.labels, labels) {
            return Err(ownership_error());
        }
        actual.labels = labels.clone();
        Ok(actual)
    }

    fn to_inspection(
        &self,
        value: &Value,
        record: &TestContainerRecord,
    ) -> Option<ContainerInspection> {
        let id = ExactContainerId::parse(record.container_id.clone()).ok()?;
        self.to_inspection_value(value, &id)
            .ok()
            .filter(|inspection| {
                inspection.name == record.name
                    && ownership_labels_match(&inspection.labels, &record.labels)
            })
            .map(|mut inspection| {
                inspection.labels = record.labels.clone();
                inspection.native_execution_began = record.native_execution_began;
                inspection.oom_killed = record.oom_killed.or(inspection.oom_killed);
                inspection.diagnostic = record.diagnostic.clone();
                inspection.cleanup_status = record.cleanup_status;
                inspection.cleanup_attempts = record.cleanup_attempts.clone();
                inspection.retryable = record
                    .cleanup_attempts
                    .last()
                    .is_some_and(|attempt| attempt.retryable);
                inspection.next_action = next_action(record.cleanup_status).into();
                inspection
            })
    }

    fn to_inspection_value(
        &self,
        value: &Value,
        id: &ExactContainerId,
    ) -> Result<ContainerInspection, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "docker inspect returned an object-shaped value".to_owned())?;
        let actual_id = object.get("Id").and_then(Value::as_str).unwrap_or_default();
        let actual_id = ExactContainerId::parse(actual_id.to_owned())
            .map_err(|_| "docker inspect returned an invalid container ID".to_owned())?;
        if &actual_id != id {
            return Err("docker inspect returned a different container ID".into());
        }
        let name = object
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_owned();
        let config = object.get("Config").and_then(Value::as_object);
        let state = object.get("State").and_then(Value::as_object);
        let labels = config
            .and_then(|config| config.get("Labels"))
            .and_then(Value::as_object)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        Ok(ContainerInspection {
            container_id: id.to_string(),
            name,
            image: config
                .and_then(|config| config.get("Image"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            state: state
                .and_then(|state| state.get("Status"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            status: state
                .and_then(|state| state.get("Status"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            labels,
            started_at: state
                .and_then(|state| state.get("StartedAt"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            finished_at: state
                .and_then(|state| state.get("FinishedAt"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            exit_code: state
                .and_then(|state| state.get("ExitCode"))
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
            signal: state
                .and_then(|state| state.get("Signal"))
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
            oom_killed: state
                .and_then(|state| state.get("OOMKilled"))
                .and_then(Value::as_bool),
            diagnostic: None,
            native_execution_began: false,
            cleanup_status: ContainerCleanupStatus::Pending,
            cleanup_attempts: Vec::new(),
            retryable: false,
            next_action: "remove through Coordinator when the check completes".into(),
        })
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "cannot format container timestamp",
                )
                .with_detail(error.to_string())
            })
    }
}

fn pending_record(
    context: &Context,
    name: String,
    image: String,
    command: Vec<String>,
    owner_token: String,
    labels: BTreeMap<String, String>,
    created_at: String,
) -> TestContainerRecord {
    TestContainerRecord {
        container_id: "0".repeat(64),
        name,
        image,
        command,
        owner_token,
        labels,
        created_at,
        run_id: context.run_id.clone(),
        check: context.check.clone(),
        repository_id: context.repository_id.clone(),
        worktree_id: context.worktree_id.clone(),
        operation_failed: false,
        native_execution_began: false,
        solver_exit_code: None,
        solver_signal: None,
        stdout_ref: None,
        stderr_ref: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
        oom_killed: None,
        diagnostic: None,
        cleanup_status: ContainerCleanupStatus::Pending,
        cleanup_attempts: Vec::new(),
    }
}

fn cleanup_details(
    record: &TestContainerRecord,
    error: &DockerError,
) -> (Option<i32>, Option<i32>, String) {
    let (exit, signal, text) = error_details(error);
    if record.labels.contains_key("devcoordinator2.component") {
        // Fixtures run with private database credentials. Keep only bounded
        // typed command facts in ordinary receipts, never subprocess text.
        (
            exit,
            signal,
            format!(
                "Docker cleanup command {:?}; inspect exact absence status",
                error.kind()
            ),
        )
    } else {
        (exit, signal, text)
    }
}

struct Context {
    #[allow(dead_code)]
    worktree: PathBuf,
    worktree_id: String,
    repository_id: String,
    records: std::fs::File,
    status: TestStatus,
    execution_uid: u32,
    uid: u32,
    gid: u32,
    run_id: String,
    check: String,
}

impl Context {
    fn unlock(&self) -> Result<(), ProtocolError> {
        rustix::fs::flock(&self.records, rustix::fs::FlockOperation::Unlock)
            .map_err(|_| state_error(crate::test_state::TestStateError::Busy))
    }
}

fn generated_name(run_id: &str, check: &str, owner: &str) -> Result<String, ProtocolError> {
    let suffix = owner.strip_prefix('b').unwrap_or(owner);
    let name = format!("devcoordinator2-test-{run_id}-{check}-{suffix}");
    if name.len() > 180 {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "run_id and check produce a container name that is too long",
        ));
    }
    Ok(name)
}

fn validate_identity(value: &str, field: &str, max: usize) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("{field} contains unsupported characters"),
        ));
    }
    Ok(())
}

fn validate_image(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > MAX_LABEL_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "image must be one bounded Docker image reference",
        ));
    }
    Ok(())
}

fn validate_command(command: &[String]) -> Result<(), ProtocolError> {
    if command.is_empty()
        || command.len() > 64
        || command
            .iter()
            .any(|part| part.is_empty() || part.len() > 1024 || part.bytes().any(|byte| byte == 0))
    {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "container command must contain 1..64 bounded argv values",
        ));
    }
    Ok(())
}

fn exact_id(value: &str) -> Result<ExactContainerId, ProtocolError> {
    ExactContainerId::parse(value.to_owned()).map_err(|_| {
        ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "container_id must be a full 64-hex ID",
        )
    })
}

fn client_name(caller: &Caller) -> String {
    serde_json::to_string(&caller.client_kind)
        .unwrap_or_else(|_| "other".into())
        .trim_matches('"')
        .to_owned()
}

fn ownership_error() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::PermissionDenied,
        "container is not an owned temporary container for this run and check",
    )
}

fn persisted_labels_match(
    actual: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> bool {
    expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

fn ownership_labels_match(
    actual: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> bool {
    persisted_labels_match(actual, expected)
        && actual
            .iter()
            .filter(|(key, _)| {
                key.strip_prefix(crate::docker::LABEL_PREFIX)
                    .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
            })
            .all(|(key, value)| expected.get(key) == Some(value))
}

fn fixture_active_error() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestStartFailed,
        "temporary container still has an active fixture lease",
    )
}

fn fixture_cleanup_deadline() -> Instant {
    Instant::now() + Duration::from_secs(FIXTURE_PHASE_TIMEOUT_SECONDS)
}

fn retryable_state(error: &ProtocolError, state: &str) -> bool {
    error
        .recovery
        .as_ref()
        .is_some_and(|recovery| recovery.retryable && recovery.state.as_deref() == Some(state))
}

fn wait_cleanup_budget(deadline: Instant, backoff: &mut Duration) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return false;
    }
    // Pace only the next authoritative receipt/lease observation. The
    // explicit ceiling is the existing bounded-observation contract.
    let pause = (*backoff).min(remaining);
    std::thread::sleep(pause.min(Duration::from_millis(100)));
    *backoff = backoff.saturating_mul(2).min(Duration::from_millis(100));
    Instant::now() < deadline
}

fn owned_cleanup_incomplete() -> String {
    "owned database cleanup is incomplete; inspect or retry its retained container record".into()
}

fn owned_cleanup_pending() -> String {
    "owned database cleanup is still pending within its retained container record".into()
}

fn cleanup_receipt_pending_error(record: &TestContainerRecord) -> ProtocolError {
    let mut recovery = devcoordinator2_api::recovery::Guidance::transient(
        "container cleanup receipt remains pending",
        "container_cleanup_receipt_pending",
    );
    recovery.retryable = false;
    recovery.waitable = false;
    recovery.run_id = Some(record.run_id.clone());
    recovery.repository_id = Some(record.repository_id.clone());
    ProtocolError::new(
        ErrorCode::TestStartFailed,
        "container cleanup receipt remains pending",
    )
    .with_recovery(recovery)
}

fn completed_start_error() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::TestStartFailed,
        "completed temporary container cannot be started",
    )
}

fn operation_busy_error(record: &TestContainerRecord) -> ProtocolError {
    let mut recovery = devcoordinator2_api::recovery::Guidance::transient(
        "temporary container operation is in progress",
        "container_operation_in_progress",
    );
    recovery.retryable = true;
    recovery.waitable = false;
    recovery.run_id = Some(record.run_id.clone());
    recovery.repository_id = Some(record.repository_id.clone());
    ProtocolError::new(
        ErrorCode::TestStartFailed,
        "temporary container operation is in progress",
    )
    .with_recovery(recovery)
}

fn docker_error(operation: &str, error: DockerError) -> ProtocolError {
    let retryable = docker_retryable(&error);
    ProtocolError::new(
        if retryable {
            ErrorCode::TestStartFailed
        } else {
            ErrorCode::InternalError
        },
        operation,
    )
    .with_detail(format!("kind={:?}; {error}", error.kind()))
    .with_recovery(devcoordinator2_api::recovery::Guidance {
        class: if retryable {
            devcoordinator2_api::recovery::Class::Transient
        } else {
            devcoordinator2_api::recovery::Class::Invalid
        },
        retryable,
        waitable: retryable,
        safe_to_continue: true,
        state: Some("container_operation_failed".into()),
        reason: error.to_string(),
        run_id: None,
        deployment_id: None,
        generation: None,
        repository_id: None,
        options: Vec::new(),
        field_errors: Vec::new(),
        example: None,
    })
}

fn docker_retryable(error: &DockerError) -> bool {
    let detail = match error.command_details().2 {
        Some(stderr) => stderr.to_ascii_lowercase(),
        None => error.to_string().to_ascii_lowercase(),
    };
    if detail.contains("permission denied")
        || detail.contains("operation not permitted")
        || detail.contains("access denied")
    {
        return false;
    }
    matches!(
        error.kind(),
        crate::docker::DockerErrorKind::CliUnavailable
            | crate::docker::DockerErrorKind::SpawnFailed
            | crate::docker::DockerErrorKind::TimedOut
            | crate::docker::DockerErrorKind::Cancelled
            | crate::docker::DockerErrorKind::CommandFailed
            | crate::docker::DockerErrorKind::NetworkUnavailable
    )
}

fn bounded_error(error: &DockerError) -> String {
    let value = error.to_string();
    value.chars().take(4_096).collect()
}

fn error_details(error: &DockerError) -> (Option<i32>, Option<i32>, String) {
    let (exit_code, signal, stderr) = error.command_details();
    (
        exit_code,
        signal,
        stderr
            .map(|value| value.to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| bounded_error(error)),
    )
}

fn unexpected_fixture_exit(inspection: &ContainerInspection) -> bool {
    let stopped = !matches!(inspection.state.as_str(), "running" | "created" | "paused");
    stopped
        && (inspection.oom_killed == Some(true)
            || inspection.exit_code != Some(0)
            || inspection.state == "dead")
}

fn valid_diagnostic(diagnostic: &ContainerDiagnostic, owner_token: &str) -> bool {
    diagnostic.reference == format!("diagnostics/{owner_token}.log")
        && diagnostic.bytes <= CONTAINER_DIAGNOSTIC_CAP as u64
        && diagnostic.sha256.len() == 64
        && diagnostic
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn next_action(status: ContainerCleanupStatus) -> &'static str {
    match status {
        ContainerCleanupStatus::Pending => "retry cleanup through Coordinator",
        ContainerCleanupStatus::Complete => "none; exact container absence was verified",
        ContainerCleanupStatus::Failed => {
            "inspect the retained cleanup error and repair authorization"
        }
    }
}

fn state_error(error: crate::test_state::TestStateError) -> ProtocolError {
    if matches!(error, crate::test_state::TestStateError::Busy) {
        let mut guidance = devcoordinator2_api::recovery::Guidance::transient(
            "container ownership metadata is busy",
            "container_metadata_busy",
        );
        guidance.retryable = true;
        guidance.waitable = false;
        return ProtocolError::new(
            ErrorCode::TestStartFailed,
            "container ownership metadata is busy; retry this exact operation",
        )
        .with_recovery(guidance);
    }
    ProtocolError::new(
        ErrorCode::InternalError,
        "governed container state is unavailable",
    )
    .with_detail(error.to_string())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "container ownership lookup failed",
    )
    .with_detail(error.to_string())
}

fn id_error(error: crate::ids::IdError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "cannot derive governed container ownership",
    )
    .with_detail(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_failures_are_terminal_cleanup_failures() {
        let error = DockerError::CommandFailure {
            message: "docker rm failed".into(),
            exit_code: Some(1),
            signal: None,
            stderr: "permission denied while removing container".into(),
        };
        assert!(!docker_retryable(&error));
    }
}
