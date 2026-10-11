//! Coordinator-owned lifecycle for temporary governed-test containers.
//!
//! Agents invoke this service over the protocol; only the daemon's Docker
//! adapter ever receives a Docker command.  Ownership is checked against the
//! authenticated worktree, the persisted run/check record, and the exact
//! managed labels before every mutation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devcoordinator2_api::ErrorCode;
use devcoordinator2_api::test_containers::{
    ContainerCleanupAttempt, ContainerCleanupStatus, ContainerCleanupSummary, ContainerCreate,
    ContainerInspection, ContainerOperationResult, ContainerReference, TestContainerRecord,
};
use devcoordinator2_api::{ProtocolError, results::TestStatus};
use rusqlite::OptionalExtension;
use serde_json::Value;
use time::{format_description::FormatItem, macros::format_description};

use crate::access::Caller;
use crate::config::Config;
use crate::database::{Database, DatabaseError};
use crate::docker::{
    CreateContainerRequest, DockerControl, DockerError, ExactContainerId, ManagedLabelContext,
};
use crate::platform::Clock;
use crate::repository::resolve_worktree;
use crate::test_state::TestRunStore;

const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
const RECORD_CAP: usize = 256;
const MAX_LABEL_BYTES: usize = 256;

#[derive(Clone)]
pub struct TestContainerService {
    database: Database,
    instance: String,
    docker: Arc<dyn DockerControl>,
    clock: Arc<dyn Clock>,
    store: TestRunStore,
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

    pub fn create(
        &self,
        params: ContainerCreate,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        validate_identity(&params.run_id, "run_id", 96)?;
        validate_identity(&params.check, "check", 96)?;
        validate_image(&params.image)?;
        validate_command(&params.command)?;
        let context = self.resolve(&params.path, &params.run_id, &params.check, caller)?;
        if context.summary.status != TestStatus::Running {
            return Err(ProtocolError::new(
                ErrorCode::TestStartFailed,
                "temporary containers can only be created for a running governed test",
            ));
        }
        let mut records = self.records(&context.current)?;
        if records.iter().any(|record| {
            record.run_id == params.run_id
                && record.check == params.check
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
        let owner_token = crate::ids::bug_id().map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "cannot allocate container owner token",
            )
            .with_detail(error.to_string())
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
        let id = match self.docker.create_container(&CreateContainerRequest {
            name: name.clone(),
            image: params.image.clone(),
            label_context,
            labels: BTreeMap::new(),
            env_file: None,
            publish: Vec::new(),
            volumes: Vec::new(),
            command: params.command.clone(),
            restart: "no".into(),
        }) {
            Ok(id) => id,
            Err(error) => {
                let (exit_code, signal, stderr) = error_details(&error);
                let stderr_bytes = stderr.len() as u64;
                let failure = TestContainerRecord {
                    container_id: "0".repeat(64),
                    name: name.clone(),
                    run_id: params.run_id.clone(),
                    check: params.check.clone(),
                    repository_id: context.repository_id.clone(),
                    worktree_id: context.worktree_id.clone(),
                    owner_token: owner_token.clone(),
                    labels: labels.clone(),
                    image: params.image.clone(),
                    command: params.command.clone(),
                    operation_failed: true,
                    created_at: created_at.clone(),
                    native_execution_began: false,
                    solver_exit_code: None,
                    solver_signal: None,
                    stdout_ref: None,
                    stderr_ref: None,
                    stdout_bytes: 0,
                    stderr_bytes,
                    cleanup_status: ContainerCleanupStatus::Failed,
                    cleanup_attempts: vec![ContainerCleanupAttempt {
                        attempt: 0,
                        attempted_at: created_at.clone(),
                        status: ContainerCleanupStatus::Failed,
                        stderr: Some(stderr),
                        stderr_bytes,
                        exit_code,
                        signal,
                        retryable: false,
                    }],
                };
                records.push(failure);
                let _ = self.write_records(&context.current, &records, context.uid, context.gid);
                return Err(docker_error("cannot create governed container", error));
            }
        };
        let inspection = match self.inspect_owned(&id, &labels, &name) {
            Ok(value) => value,
            Err(error) => {
                let cleanup = self
                    .docker
                    .remove_container_verified_named(&id, &name, true);
                let detail = match cleanup {
                    Ok(()) => {
                        "created container failed ownership inspection and was removed".into()
                    }
                    Err(cleanup_error) => format!(
                        "created container failed ownership inspection; cleanup failed: {cleanup_error}"
                    ),
                };
                return Err(error.with_detail(detail));
            }
        };
        let record = TestContainerRecord {
            container_id: id.to_string(),
            name,
            run_id: params.run_id,
            check: params.check,
            repository_id: context.repository_id,
            worktree_id: context.worktree_id,
            owner_token,
            labels,
            image: params.image,
            command: params.command,
            operation_failed: false,
            created_at,
            native_execution_began: false,
            solver_exit_code: None,
            solver_signal: None,
            stdout_ref: None,
            stderr_ref: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            cleanup_status: ContainerCleanupStatus::Pending,
            cleanup_attempts: Vec::new(),
        };
        records.push(record.clone());
        if let Err(error) = self.write_records(&context.current, &records, context.uid, context.gid)
        {
            let _ = self
                .docker
                .remove_container_verified_named(&id, &record.name, true);
            return Err(error);
        }
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
        let context = self.resolve(&params.path, &params.run_id, &params.check, caller)?;
        let (mut records, index) = self.find_record(&context, &params)?;
        let id = exact_id(&params.container_id)?;
        if records[index].container_id != id.as_str() {
            return Err(ownership_error());
        }
        self.verify_record(&records[index], &context)?;
        if let Err(error) = self.inspect_owned(&id, &records[index].labels, &records[index].name) {
            records[index].operation_failed = true;
            records[index].cleanup_status = ContainerCleanupStatus::Failed;
            records[index]
                .cleanup_attempts
                .push(ContainerCleanupAttempt {
                    attempt: 0,
                    attempted_at: self.timestamp()?,
                    status: ContainerCleanupStatus::Failed,
                    stderr: Some("container ownership inspection failed".into()),
                    stderr_bytes: "container ownership inspection failed".len() as u64,
                    exit_code: None,
                    signal: None,
                    retryable: false,
                });
            let _ = self.write_records(&context.current, &records, context.uid, context.gid);
            return Err(error);
        }
        if let Err(error) = self.docker.start_container(&id) {
            let (exit_code, signal, stderr) = error_details(&error);
            let stderr_bytes = stderr.len() as u64;
            records[index].operation_failed = true;
            records[index].cleanup_status = ContainerCleanupStatus::Pending;
            records[index]
                .cleanup_attempts
                .push(ContainerCleanupAttempt {
                    attempt: 0,
                    attempted_at: self.timestamp()?,
                    status: ContainerCleanupStatus::Failed,
                    stderr: Some(stderr),
                    stderr_bytes,
                    exit_code,
                    signal,
                    retryable: false,
                });
            self.write_records(&context.current, &records, context.uid, context.gid)?;
            return Err(docker_error("cannot start governed container", error));
        }
        records[index].native_execution_began = true;
        self.write_records(&context.current, &records, context.uid, context.gid)?;
        let inspection = self.inspect_owned(&id, &records[index].labels, &records[index].name)?;
        records[index].solver_exit_code = inspection.exit_code;
        records[index].solver_signal = inspection.signal;
        self.write_records(&context.current, &records, context.uid, context.gid)?;
        Ok(ContainerOperationResult {
            operation: "started".into(),
            record: records[index].clone(),
            inspection: Some(inspection),
        })
    }

    pub fn inspect(
        &self,
        params: ContainerReference,
        caller: &Caller,
    ) -> Result<ContainerOperationResult, ProtocolError> {
        let context = self.resolve(&params.path, &params.run_id, &params.check, caller)?;
        let (mut records, index) = self.find_record(&context, &params)?;
        let id = exact_id(&params.container_id)?;
        if records[index].container_id != id.as_str() {
            return Err(ownership_error());
        }
        self.verify_record(&records[index], &context)?;
        let inspection = self.inspect_owned(&id, &records[index].labels, &records[index].name)?;
        records[index].solver_exit_code = inspection.exit_code;
        records[index].solver_signal = inspection.signal;
        self.write_records(&context.current, &records, context.uid, context.gid)?;
        Ok(ContainerOperationResult {
            operation: "inspected".into(),
            record: records[index].clone(),
            inspection: Some(inspection),
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
        let mut records = self.records(&context.current)?;
        let mut native_execution_started = false;
        for record in records.iter_mut().filter(|record| {
            record.run_id == run_id
                && record.repository_id == context.repository_id
                && record.worktree_id == context.worktree_id
        }) {
            native_execution_started |= record.native_execution_began;
            if record.cleanup_status == ContainerCleanupStatus::Complete {
                continue;
            }
            self.verify_record_labels(record)?;
            let id = exact_id(&record.container_id)?;
            let attempt = u32::try_from(record.cleanup_attempts.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            let attempted_at = self.timestamp()?;
            let removal = self
                .docker
                .remove_container_verified_named(&id, &record.name, true);
            let (status, retryable, diagnostics) = match removal {
                Ok(()) => (ContainerCleanupStatus::Complete, false, None),
                Err(error) => {
                    let retryable = docker_retryable(&error);
                    (
                        if retryable {
                            ContainerCleanupStatus::Pending
                        } else {
                            ContainerCleanupStatus::Failed
                        },
                        retryable,
                        Some(error_details(&error)),
                    )
                }
            };
            record.cleanup_status = status;
            record.cleanup_attempts.push(ContainerCleanupAttempt {
                attempt,
                attempted_at,
                status,
                stderr: diagnostics.as_ref().map(|(_, _, value)| value.clone()),
                stderr_bytes: diagnostics
                    .as_ref()
                    .map_or(0, |(_, _, value)| value.len() as u64),
                exit_code: diagnostics.as_ref().and_then(|(code, _, _)| *code),
                signal: diagnostics.as_ref().and_then(|(_, signal, _)| *signal),
                retryable,
            });
        }
        self.write_records(&context.current, &records, context.uid, context.gid)?;
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
        let context = self.resolve(&params.path, &params.run_id, &params.check, caller)?;
        let (mut records, index) = self.find_record(&context, &params)?;
        let id = exact_id(&params.container_id)?;
        if records[index].container_id != id.as_str() {
            return Err(ownership_error());
        }
        self.verify_record(&records[index], &context)?;
        if let Ok(inspection) =
            self.inspect_owned(&id, &records[index].labels, &records[index].name)
        {
            records[index].solver_exit_code = inspection.exit_code;
            records[index].solver_signal = inspection.signal;
        }
        let attempt = u32::try_from(records[index].cleanup_attempts.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let attempted_at = self.timestamp()?;
        let removal = self
            .docker
            .remove_container_verified_named(&id, &records[index].name, true);
        let (status, retryable, diagnostics) = match removal {
            Ok(()) => (ContainerCleanupStatus::Complete, false, None),
            Err(error) => {
                let retryable = docker_retryable(&error);
                (
                    if retryable {
                        ContainerCleanupStatus::Pending
                    } else {
                        ContainerCleanupStatus::Failed
                    },
                    retryable,
                    Some(error_details(&error)),
                )
            }
        };
        records[index].cleanup_status = status;
        records[index]
            .cleanup_attempts
            .push(ContainerCleanupAttempt {
                attempt,
                attempted_at,
                status,
                stderr: diagnostics.as_ref().map(|(_, _, value)| value.clone()),
                stderr_bytes: diagnostics
                    .as_ref()
                    .map_or(0, |(_, _, value)| value.len() as u64),
                exit_code: diagnostics.as_ref().and_then(|(code, _, _)| *code),
                signal: diagnostics.as_ref().and_then(|(_, signal, _)| *signal),
                retryable,
            });
        self.write_records(&context.current, &records, context.uid, context.gid)?;
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
        if path.is_empty() || !Path::new(path).is_absolute() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "path must be an absolute registered worktree path",
            ));
        }
        let (worktree, worktree_id, repository_id) = if caller.is_console() {
            let requested = path.to_owned();
            self.database
                .call(move |connection| {
                    connection
                        .query_row(
                            "SELECT worktree_path,worktree_id,repository_id FROM worktrees WHERE worktree_path=?1",
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
        let current = self
            .store
            .open_current(&worktree)
            .map_err(state_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::TestNotFound,
                    "governed test state is unavailable",
                )
            })?;
        let summary = self
            .store
            .read_current_summary(&worktree)
            .map_err(state_error)?
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::TestNotFound, "governed test run not found")
            })?;
        if summary.run_id != run_id {
            return Err(ProtocolError::new(
                ErrorCode::TestNotFound,
                "run_id does not match the current governed test",
            ));
        }
        let (uid, gid) = self.store.owner(&current).map_err(state_error)?;
        Ok(Context {
            worktree,
            worktree_id,
            repository_id,
            current,
            summary,
            execution_uid: uid,
            uid,
            gid,
            run_id: run_id.into(),
            check: check.into(),
        })
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
        let records = self.records(&context.current)?;
        let index = records
            .iter()
            .position(|record| {
                record.container_id == params.container_id
                    && record.run_id == context.run_id
                    && record.check == context.check
                    && record.repository_id == context.repository_id
                    && record.worktree_id == context.worktree_id
            })
            .ok_or_else(ownership_error)?;
        Ok((records, index))
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
        self.verify_record_labels(record)
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
        let actual = self
            .to_inspection_value(&value, id)
            .map_err(|message| ProtocolError::new(ErrorCode::TestStartFailed, message))?;
        if actual.name != expected_name || actual.labels != *labels {
            return Err(ownership_error());
        }
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
            .map(|mut inspection| {
                inspection.native_execution_began = record.native_execution_began;
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

struct Context {
    #[allow(dead_code)]
    worktree: PathBuf,
    worktree_id: String,
    repository_id: String,
    current: std::fs::File,
    summary: devcoordinator2_api::results::TestSummary,
    execution_uid: u32,
    uid: u32,
    gid: u32,
    run_id: String,
    check: String,
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
