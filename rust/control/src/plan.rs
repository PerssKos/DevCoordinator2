//! Permanent planning ledger and release-delivery evidence.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use devcoordinator2_api::params::{
    DecisionRecord, DecisionSearch as DecisionSearchParams, DecisionSummarize,
    DecisionTail as DecisionTailParams, ReleaseCreate, ReleaseDeliver, ReleaseKind, ReleaseRequest,
    ReleaseStatus, ReleaseUpdate, TaskCreate, TaskKind, TaskStatus, TaskUpdate,
};
use devcoordinator2_api::results::{
    CurrentReleaseSummary, Decision, DecisionRecorded, DecisionSearch, DecisionState,
    DecisionSummarized, DecisionSummary, DecisionTail, ElaborationRequest, PlanCollection,
    PlanDetail, PlanEvent, PlanOverview, PlanRelease, PlanRepositoryRow, PlanTask, PreviewRequest,
    ReleaseDelivered, ReleaseMutation, Task, TaskCreated, TaskHistory, TaskMutation,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::database::{Database, DatabaseError};
use crate::ids;

const SUMMARY_DUE_THRESHOLD: u32 = 25;
const HISTORY_EVENT_CAP: u32 = 200;
const OVERVIEW_TASK_CAP: usize = 500;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryEvidence {
    pub deployment_id: String,
    pub repository_id: String,
    pub generation_number: u32,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub fingerprint: Option<String>,
    pub url: Option<String>,
    pub port: u16,
}

pub trait DeploymentEvidenceReader: Send + Sync {
    fn read(&self, deployment_id: &str) -> Result<DeliveryEvidence, ProtocolError>;
}

#[derive(Clone)]
pub struct SqliteDeploymentEvidence {
    database: Database,
    base_domain: Arc<str>,
}

impl SqliteDeploymentEvidence {
    pub fn new(database: Database, base_domain: impl Into<Arc<str>>) -> Self {
        Self {
            database,
            base_domain: base_domain.into(),
        }
    }
}

#[derive(Debug)]
struct DeploymentRow {
    repository_id: String,
    current_generation: Option<u32>,
    spec_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskRow {
    task_id: String,
    repository_id: String,
    parent_task_id: Option<String>,
    release_id: Option<String>,
    seq: u32,
    position: u32,
    title: String,
    outcome: String,
    impact: Option<String>,
    unblock_condition: Option<String>,
    verification: Option<String>,
    technical_note: Option<String>,
    kind: String,
    status: String,
    estimated_loc: Option<u32>,
    elaboration_needed: bool,
    created_at: String,
    created_by: String,
    updated_at: String,
}

#[derive(Clone, Debug)]
struct ReleaseRow {
    release_id: String,
    repository_id: String,
    seq: u32,
    name: String,
    kind: String,
    status: String,
    note: Option<String>,
    requested_at: Option<String>,
}

#[derive(Clone, Debug)]
struct ReleaseOverviewRow {
    release_id: String,
    seq: u32,
    name: String,
    kind: String,
    status: String,
    note: Option<String>,
    requested_at: Option<String>,
    delivered_at: Option<String>,
    url: Option<String>,
    port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Aggregate {
    tasks_total: u32,
    tasks_done: u32,
    loc_total: u64,
    loc_done: u64,
}

#[derive(Debug, Deserialize)]
struct StoredSpec {
    #[serde(default)]
    components: Vec<StoredComponent>,
}

#[derive(Debug, Deserialize)]
struct StoredComponent {
    name: String,
    #[serde(rename = "type")]
    component_type: String,
    #[serde(default)]
    wants_port: bool,
    #[serde(default)]
    route: bool,
}

impl DeploymentEvidenceReader for SqliteDeploymentEvidence {
    fn read(&self, deployment_id: &str) -> Result<DeliveryEvidence, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        let base_domain = Arc::clone(&self.base_domain);
        self.database
            .call(move |connection| {
                let deployment = connection
                    .query_row(
                        "SELECT repository_id,current_generation,spec_json FROM deployments WHERE deployment_id=?1",
                        [&deployment_id],
                        |row| {
                            Ok(DeploymentRow {
                                repository_id: row.get(0)?,
                                current_generation: row.get(1)?,
                                spec_json: row.get(2)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(deployment) = deployment else {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        format!("no deployment {deployment_id}"),
                    ));
                };
                let Some(generation_number) = deployment.current_generation else {
                    return Err(domain_error(
                        ErrorCode::ParamsInvalid,
                        "the deployment has no running generation; apply it before delivery",
                    ));
                };
                let generation = connection
                    .query_row(
                        "SELECT commit_hash,dirty,fingerprint FROM generations WHERE deployment_id=?1 AND number=?2",
                        rusqlite::params![deployment_id, generation_number],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, i64>(1)? != 0,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let (commit_hash, dirty, fingerprint) = generation.unwrap_or((None, false, None));

                let route = connection
                    .query_row(
                        "SELECT domain,component,port FROM domain_routes WHERE deployment_id=?1 ORDER BY domain LIMIT 1",
                        [&deployment_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<u16>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let (domain, component, routed_port) = match route {
                    Some((domain, component, port)) => (Some(domain), Some(component), port),
                    None => (None, None, None),
                };
                let port = match routed_port {
                    Some(port) => port,
                    None => {
                        let component = match component {
                            Some(component) => component,
                            None => route_component(&deployment.spec_json)?,
                        };
                        connection
                            .query_row(
                                "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation IN (?3,0) ORDER BY generation DESC LIMIT 1",
                                rusqlite::params![deployment_id, component, generation_number],
                                |row| row.get::<_, u16>(0),
                            )
                            .optional()?
                            .ok_or_else(|| {
                                domain_error(
                                    ErrorCode::ParamsInvalid,
                                    "the declared route component has no leased port",
                                )
                            })?
                    }
                };
                let url = domain.map(|label| {
                    let host = if base_domain.is_empty() {
                        label
                    } else {
                        format!("{label}.{base_domain}")
                    };
                    format!("https://{host}")
                });
                Ok(DeliveryEvidence {
                    deployment_id,
                    repository_id: deployment.repository_id,
                    generation_number,
                    commit_hash,
                    dirty,
                    fingerprint,
                    url,
                    port,
                })
            })
            .map_err(database_or_domain)
    }
}

fn route_component(spec_json: &str) -> Result<String, DatabaseError> {
    let spec: StoredSpec = serde_json::from_str(spec_json).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(
                ErrorCode::InternalError,
                "stored deployment specification is invalid",
            )
            .with_detail(error.to_string()),
        )
    })?;
    if let Some(component) = spec.components.iter().find(|component| component.route) {
        return Ok(component.name.clone());
    }
    let candidates = spec
        .components
        .iter()
        .filter(|component| {
            component.wants_port
                && matches!(
                    component.component_type.as_str(),
                    "process" | "docker" | "compose"
                )
        })
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        return Ok(candidates[0].name.clone());
    }
    Err(domain_error(
        ErrorCode::ParamsInvalid,
        "the deployment has no unambiguous declared route component",
    ))
}

#[derive(Clone)]
pub struct PlanService {
    database: Database,
    deployments: Arc<dyn DeploymentEvidenceReader>,
}

impl PlanService {
    pub fn new(database: Database, deployments: Arc<dyn DeploymentEvidenceReader>) -> Self {
        Self {
            database,
            deployments,
        }
    }

    pub fn overview(&self, repository_id: Option<&str>) -> Result<PlanOverview, ProtocolError> {
        match repository_id {
            Some(repository_id) => self.plan_detail(repository_id).map(PlanOverview::Detail),
            None => self.plan_collection().map(PlanOverview::Collection),
        }
    }

    fn plan_collection(&self) -> Result<PlanCollection, ProtocolError> {
        let repositories = self
            .database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT r.repository_id,r.display_name,p.display_name,p.icon,r.root_path FROM repositories r \
                     LEFT JOIN repository_presentation p ON p.repository_id=r.repository_id \
                     WHERE r.archived_at IS NULL ORDER BY r.display_name,r.repository_id",
                )?;
                Ok(statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_or_domain)?;
        let mut rows = Vec::with_capacity(repositories.len());
        for (repository_id, display_name, custom_name, icon, root_path) in repositories {
            let presentation = (custom_name.is_some() || icon.is_some()).then(|| {
                devcoordinator2_api::results::RepositoryPresentation {
                    repository_id: repository_id.clone(),
                    display_name: custom_name,
                    icon,
                }
            });
            let repository_id_for_query = repository_id.clone();
            let (aggregate, current_release, elaboration_request_count, preview_requested) = self
                .database
                .call(move |connection| {
                    let tasks = read_tasks(connection, &repository_id_for_query, true)?;
                    let (aggregates, _) = leaf_aggregates(&tasks);
                    let aggregate = aggregates.values().fold(Aggregate::default(), |mut total, row| {
                        total.tasks_total += row.tasks_total;
                        total.tasks_done += row.tasks_done;
                        total.loc_total += row.loc_total;
                        total.loc_done += row.loc_done;
                        total
                    });
                    let releases = read_release_overviews(connection, &repository_id_for_query)?;
                    let current_release = releases
                        .iter()
                        .find(|release| matches!(release.status.as_str(), "planned" | "requested"))
                        .or_else(|| releases.last())
                        .map(|release| -> Result<CurrentReleaseSummary, DatabaseError> {
                            Ok(CurrentReleaseSummary {
                                name: release.name.clone(),
                                kind: parse_release_kind(&release.kind)?,
                                status: parse_release_status(&release.status)?,
                            })
                        })
                        .transpose()?;
                    let elaboration_request_count = connection.query_row(
                        "SELECT COUNT(*) FROM tasks WHERE repository_id=?1 AND elaboration_needed=1",
                        [&repository_id_for_query],
                        |row| row.get::<_, u32>(0),
                    )?;
                    let preview_requested = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM releases WHERE repository_id=?1 AND status='requested')",
                        [&repository_id_for_query],
                        |row| row.get::<_, i64>(0),
                    )? != 0;
                    Ok((aggregate, current_release, elaboration_request_count, preview_requested))
                })
                .map_err(database_or_domain)?;
            rows.push(PlanRepositoryRow {
                repository_id,
                display_name,
                repository_source: crate::repository::repository_source_for_root(
                    std::path::Path::new(&root_path),
                ),
                root_path,
                presentation,
                open_tasks: aggregate.tasks_total.saturating_sub(aggregate.tasks_done),
                loc_done: aggregate.loc_done,
                loc_total: aggregate.loc_total,
                current_release,
                preview_requested,
                elaboration_request_count,
            });
        }
        Ok(PlanCollection { repositories: rows })
    }

    fn plan_detail(&self, repository_id: &str) -> Result<PlanDetail, ProtocolError> {
        let repository_id_owned = repository_id.to_owned();
        let (display_name, archived, replacement, releases, mut tasks) = self
            .database
            .call(move |connection| {
                let repository = connection
                    .query_row(
                        "SELECT display_name,archived_at IS NOT NULL,merged_into_repository_id FROM repositories WHERE repository_id=?1",
                        [&repository_id_owned],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)? != 0,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((display_name, archived, replacement)) = repository else {
                    return Err(domain_error(
                        ErrorCode::RepositoryNotFound,
                        "repository is not registered",
                    ));
                };
                Ok((
                    display_name,
                    archived,
                    replacement,
                    read_release_overviews(connection, &repository_id_owned)?,
                    read_tasks(connection, &repository_id_owned, false)?,
                ))
            })
            .map_err(database_or_domain)?;
        let (aggregates, _) = leaf_aggregates(&tasks);
        let releases = releases
            .into_iter()
            .map(|release| {
                let aggregate = aggregates
                    .get(&Some(release.release_id.clone()))
                    .copied()
                    .unwrap_or_default();
                Ok(PlanRelease {
                    release_id: release.release_id,
                    name: release.name,
                    kind: parse_release_kind(&release.kind).map_err(database_or_domain)?,
                    status: parse_release_status(&release.status).map_err(database_or_domain)?,
                    seq: release.seq,
                    note: release.note,
                    requested_at: release.requested_at,
                    delivered_at: release.delivered_at,
                    url: release.url,
                    port: release.port,
                    tasks_total: aggregate.tasks_total,
                    tasks_done: aggregate.tasks_done,
                    loc_total: aggregate.loc_total,
                    loc_done: aggregate.loc_done,
                })
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        let tasks_truncated = tasks.len() > OVERVIEW_TASK_CAP;
        if tasks_truncated {
            let mut open = tasks
                .iter()
                .filter(|task| task.status != "done")
                .cloned()
                .collect::<Vec<_>>();
            let done = tasks
                .into_iter()
                .filter(|task| task.status == "done")
                .collect::<Vec<_>>();
            let remaining = OVERVIEW_TASK_CAP.saturating_sub(open.len());
            open.extend(done.into_iter().rev().take(remaining));
            open.sort_by_key(|task| task.seq);
            tasks = open;
        }
        let tasks = tasks
            .into_iter()
            .map(|task| {
                Ok(PlanTask {
                    task_id: task.task_id,
                    parent_task_id: task.parent_task_id,
                    release_id: task.release_id,
                    seq: task.seq,
                    position: task.position,
                    title: task.title,
                    impact: task.impact.map(|impact| clip(&impact, 160)),
                    status: parse_task_status(&task.status).map_err(database_or_domain)?,
                    kind: parse_task_kind(&task.kind).map_err(database_or_domain)?,
                    estimated_loc: task.estimated_loc,
                    elaboration_needed: task.elaboration_needed,
                })
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        let repository_id = repository_id.to_owned();
        let preview_requested = self.preview_requests(&repository_id)?;
        let unsummarized_count = self.unsummarized_count(&repository_id)?;
        Ok(PlanDetail {
            repository_id: repository_id.clone(),
            display_name,
            archived,
            merged_into_repository_id: replacement,
            releases,
            tasks,
            tasks_truncated,
            elaboration_requests: self.elaboration_requests(&repository_id)?,
            preview_requested,
            decisions: DecisionState {
                unsummarized_count,
                summary_due: unsummarized_count >= SUMMARY_DUE_THRESHOLD,
            },
        })
    }

    pub fn create_task(
        &self,
        repository_id: &str,
        params: TaskCreate,
        actor: &str,
        now: &str,
    ) -> Result<TaskCreated, ProtocolError> {
        self.require_active_repository(repository_id)?;
        validate_plain_line("title", &params.title, 3, 120)?;
        let title = params.title.trim().to_owned();
        let outcome = match params.outcome {
            Some(value) => {
                validate_plain_text("outcome", &value, 10, 2000)?;
                value.trim().to_owned()
            }
            None => title.clone(),
        };
        for (name, value, maximum) in [
            ("impact", params.impact.as_deref(), 2000),
            (
                "unblock_condition",
                params.unblock_condition.as_deref(),
                2000,
            ),
            ("verification", params.verification.as_deref(), 2000),
            ("technical_note", params.technical_note.as_deref(), 4000),
        ] {
            if let Some(value) = value {
                validate_plain_text(name, value, 1, maximum)?;
            }
        }
        if let Some(parent) = &params.parent_task_id {
            let row = self.task_row(parent)?.ok_or_else(|| {
                ProtocolError::new(ErrorCode::TaskNotFound, format!("no task {parent}"))
            })?;
            if row.repository_id != repository_id {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "the parent task belongs to another repository",
                ));
            }
        }
        if let Some(release_id) = &params.release_id {
            self.require_open_release(repository_id, release_id)?;
        }
        let task_id = ids::task_id().map_err(id_error)?;
        let task_id_for_insert = task_id.clone();
        let repository_id_owned = repository_id.to_owned();
        let parent_task_id = params.parent_task_id.clone();
        let release_id = params.release_id.clone();
        let impact = params.impact.map(|value| value.trim().to_owned());
        let unblock_condition = params
            .unblock_condition
            .map(|value| value.trim().to_owned());
        let verification = params.verification.map(|value| value.trim().to_owned());
        let technical_note = params.technical_note.map(|value| value.trim().to_owned());
        let kind = enum_string(&params.kind)?;
        let estimated_loc = params.estimated_loc;
        let actor = actor.to_owned();
        let now = now.to_owned();
        let (seq, position) = self
            .database
            .transaction(move |transaction| {
                let seq: u32 = transaction.query_row(
                    "SELECT COALESCE(MAX(seq),0)+1 FROM tasks WHERE repository_id=?1",
                    [&repository_id_owned],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "INSERT INTO tasks(task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,?5,0,?6,?7,?8,?9,?10,?11,?12,'planned',?13,?14,?15,?14)",
                    rusqlite::params![
                        task_id_for_insert,
                        repository_id_owned,
                        parent_task_id,
                        release_id,
                        seq,
                        title,
                        outcome,
                        impact,
                        unblock_condition,
                        verification,
                        technical_note,
                        kind,
                        estimated_loc,
                        now,
                        actor,
                    ],
                )?;
                let position = place_task(
                    transaction,
                    &repository_id_owned,
                    parent_task_id.as_deref(),
                    release_id.as_deref(),
                    &task_id_for_insert,
                    None,
                )?;
                append_plan_event(
                    transaction,
                    &repository_id_owned,
                    "task",
                    &task_id_for_insert,
                    "created",
                    None,
                    Some(&kind),
                    &actor,
                    &now,
                    None,
                )?;
                Ok((seq, position))
            })
            .map_err(database_or_domain)?;
        Ok(TaskCreated {
            task_id,
            repository_id: repository_id.to_owned(),
            seq,
            position,
            status: TaskStatus::Planned,
            release_id: params.release_id,
            elaboration_needed: false,
            preview_requested: self.has_requested(repository_id)?,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    pub fn search_tasks(
        &self,
        repository_id: &str,
        params: devcoordinator2_api::params::TaskSearch,
    ) -> Result<devcoordinator2_api::results::TaskSearch, ProtocolError> {
        use devcoordinator2_api::results::{TaskSearch, TaskSearchHit};
        if params.query.len() > 256
            || !(1..=50).contains(&params.limit)
            || params.after_sequence > i64::MAX as u64
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "task search requires a query up to 256 bytes, limit 1..50, and a valid sequence",
            ));
        }
        let repository_id = repository_id.to_owned();
        let status = params.status.map(|status| match status {
            TaskStatus::Planned => "planned",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Done => "done",
            TaskStatus::Dropped => "dropped",
        });
        let limit = usize::from(params.limit);
        let mut tasks = self.database.call(move |connection| {
            let mut statement = connection.prepare("SELECT task_id,seq,title,status FROM tasks WHERE repository_id=?1 AND (?2 IS NULL OR status=?2) AND seq>?3 AND instr(lower(title || ' ' || coalesce(outcome,'') || ' ' || coalesce(technical_note,'')),lower(?4))>0 ORDER BY seq LIMIT ?5")?;
            let rows = statement.query_map(rusqlite::params![repository_id, status, params.after_sequence as i64, params.query, i64::from(params.limit)+1], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })?.collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().map(|(task_id, sequence, title, status)| Ok(TaskSearchHit { task_id, sequence: sequence as u64, title, status: parse_task_status(&status)? })).collect::<Result<Vec<_>, DatabaseError>>()
        }).map_err(database_or_domain)?;
        let truncated = tasks.len() > limit;
        tasks.truncate(limit);
        let next_sequence = truncated.then(|| tasks.last().expect("positive page size").sequence);
        Ok(TaskSearch {
            tasks,
            next_sequence,
        })
    }

    pub fn task_history(&self, task_id: &str) -> Result<TaskHistory, ProtocolError> {
        let row = self.task_row(task_id)?.ok_or_else(|| {
            ProtocolError::new(ErrorCode::TaskNotFound, format!("no task {task_id}"))
        })?;
        let task = task_result(row.clone())?;
        let task_id = task_id.to_owned();
        let (events, events_truncated) = self
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT event,from_value,to_value,actor,at,note FROM plan_events WHERE subject_kind='task' AND subject_id=?1 ORDER BY event_id DESC LIMIT ?2",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![task_id, HISTORY_EVENT_CAP + 1], |row| {
                        Ok(PlanEvent {
                            event: row.get(0)?,
                            from: row.get(1)?,
                            to: row.get(2)?,
                            actor: row.get(3)?,
                            at: row.get(4)?,
                            note: row.get(5)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let truncated = rows.len() > HISTORY_EVENT_CAP as usize;
                let events = rows
                    .into_iter()
                    .take(HISTORY_EVENT_CAP as usize)
                    .rev()
                    .collect();
                Ok((events, truncated))
            })
            .map_err(database_or_domain)?;
        Ok(TaskHistory {
            task,
            events,
            events_truncated,
            elaboration_requests: self.elaboration_requests(&row.repository_id)?,
        })
    }

    pub fn update_task(
        &self,
        params: TaskUpdate,
        actor: &str,
        now: &str,
    ) -> Result<TaskMutation, ProtocolError> {
        let mut task = self.task_row(&params.task_id)?.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::TaskNotFound,
                format!("no task {}", params.task_id),
            )
        })?;
        self.require_active_repository(&task.repository_id)?;
        let old = task.clone();
        let mut events: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        let mut edited = Vec::new();
        let mut elaboration_event = None;
        if let Some(value) = params.title {
            validate_plain_line("title", &value, 3, 120)?;
            let value = value.trim().to_owned();
            if value != task.title {
                task.title = value;
                edited.push("title");
            }
        }
        macro_rules! update_text {
            ($field:ident, $value:expr, $maximum:expr) => {
                if let Some(value) = $value {
                    validate_plain_text(stringify!($field), &value, 1, $maximum)?;
                    let value = value.trim().to_owned();
                    if task.$field.as_deref() != Some(&value) {
                        task.$field = Some(value);
                        edited.push(stringify!($field));
                    }
                }
            };
        }
        if let Some(value) = params.outcome {
            validate_plain_text("outcome", &value, 10, 2000)?;
            let value = value.trim().to_owned();
            if value != task.outcome {
                task.outcome = value;
                edited.push("outcome");
            }
        }
        update_text!(impact, params.impact, 2000);
        update_text!(unblock_condition, params.unblock_condition, 2000);
        update_text!(verification, params.verification, 2000);
        update_text!(technical_note, params.technical_note, 4000);
        if let Some(value) = params.estimated_loc
            && task.estimated_loc != Some(value)
        {
            events.push((
                "estimate".into(),
                task.estimated_loc.map(|number| number.to_string()),
                Some(value.to_string()),
            ));
            task.estimated_loc = Some(value);
        }
        if let Some(value) = params.status {
            let value = enum_string(&value)?;
            if value != task.status {
                events.push((
                    "status".into(),
                    Some(task.status.clone()),
                    Some(value.clone()),
                ));
                task.status = value;
            }
        }
        let mut regroup = false;
        if let Some(value) = params.release_id {
            if let Some(release_id) = &value {
                self.require_open_release(&task.repository_id, release_id)?;
            }
            if value != task.release_id {
                events.push((
                    "release_move".into(),
                    task.release_id.clone(),
                    value.clone(),
                ));
                task.release_id = value;
                regroup = true;
            }
        }
        if let Some(value) = params.parent_task_id {
            if let Some(parent_id) = &value {
                let parent = self.task_row(parent_id)?.ok_or_else(|| {
                    ProtocolError::new(ErrorCode::TaskNotFound, format!("no task {parent_id}"))
                })?;
                if parent.repository_id != task.repository_id {
                    return Err(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "the parent task belongs to another repository",
                    ));
                }
                if self.would_create_cycle(&task.task_id, parent_id)? {
                    return Err(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "that move would make the task its own ancestor",
                    ));
                }
            }
            if value != task.parent_task_id {
                events.push((
                    "reparent".into(),
                    task.parent_task_id.clone(),
                    value.clone(),
                ));
                task.parent_task_id = value;
                regroup = true;
            }
        }
        if params.position.is_some() || (old.status == "dropped" && task.status != "dropped") {
            regroup = true;
        }
        if let Some(elaboration) = params.elaboration_needed
            && elaboration != task.elaboration_needed
        {
            if !elaboration
                && !edited
                    .iter()
                    .any(|field| matches!(*field, "title" | "outcome"))
            {
                return Err(ProtocolError::new(
                    ErrorCode::ParamsInvalid,
                    "clearing elaboration_needed requires a changed title or outcome",
                ));
            }
            elaboration_event = Some((
                if elaboration {
                    "elaboration_requested"
                } else {
                    "elaboration_completed"
                }
                .into(),
                Some(task.elaboration_needed.to_string()),
                Some(elaboration.to_string()),
            ));
            task.elaboration_needed = elaboration;
        }
        if !edited.is_empty() {
            edited.sort_unstable();
            events.push(("edited".into(), None, Some(edited.join(","))));
        }
        if let Some(event) = elaboration_event {
            events.push(event);
        }
        if task == old && params.position.is_none() {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "nothing to change",
            ));
        }
        let repository_id = task.repository_id.clone();
        let transaction_repository_id = repository_id.clone();
        let task_id = task.task_id.clone();
        let actor = actor.to_owned();
        let now = now.to_owned();
        let note = params.note.map(|value| value.trim().to_owned());
        let requested_position = params.position;
        let mut updated = task.clone();
        let transaction_updated = updated.clone();
        let position = self
            .database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE tasks SET parent_task_id=?1,release_id=?2,title=?3,outcome=?4,impact=?5,unblock_condition=?6,verification=?7,technical_note=?8,status=?9,estimated_loc=?10,elaboration_needed=?11,updated_at=?12 WHERE task_id=?13",
                    rusqlite::params![
                        transaction_updated.parent_task_id,
                        transaction_updated.release_id,
                        transaction_updated.title,
                        transaction_updated.outcome,
                        transaction_updated.impact,
                        transaction_updated.unblock_condition,
                        transaction_updated.verification,
                        transaction_updated.technical_note,
                        transaction_updated.status,
                        transaction_updated.estimated_loc,
                        i64::from(transaction_updated.elaboration_needed),
                        now,
                        task_id,
                    ],
                )?;
                let position = if regroup && transaction_updated.status != "dropped" {
                    place_task(
                        transaction,
                        &transaction_repository_id,
                        transaction_updated.parent_task_id.as_deref(),
                        transaction_updated.release_id.as_deref(),
                        &task_id,
                        requested_position,
                    )?
                } else {
                    transaction_updated.position
                };
                if requested_position.is_some() && position != old.position {
                    events.push((
                        "reorder".into(),
                        Some(old.position.to_string()),
                        Some(position.to_string()),
                    ));
                }
                for (event, from, to) in events {
                    append_plan_event(
                        transaction,
                        &transaction_repository_id,
                        "task",
                        &task_id,
                        &event,
                        from.as_deref(),
                        to.as_deref(),
                        &actor,
                        &now,
                        note.as_deref(),
                    )?;
                }
                Ok(position)
            })
            .map_err(database_or_domain)?;
        updated.position = position;
        task_mutation(
            updated,
            self.has_requested(&repository_id)?,
            self.elaboration_requests(&repository_id)?,
        )
    }

    pub fn create_release(
        &self,
        repository_id: &str,
        params: ReleaseCreate,
        actor: &str,
        now: &str,
    ) -> Result<ReleaseMutation, ProtocolError> {
        self.require_active_repository(repository_id)?;
        validate_plain_line("name", &params.name, 3, 120)?;
        if let Some(note) = &params.note {
            validate_plain_text("note", note, 1, 500)?;
        }
        self.insert_release(
            repository_id,
            params.name.trim(),
            params.kind,
            ReleaseStatus::Planned,
            params.note.as_deref().map(str::trim),
            params.seq,
            actor,
            now,
        )
    }

    pub fn request_release(
        &self,
        repository_id: &str,
        params: ReleaseRequest,
        actor: &str,
        now: &str,
    ) -> Result<ReleaseMutation, ProtocolError> {
        self.require_active_repository(repository_id)?;
        if self.has_requested(repository_id)? {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "a preview is already requested for this repository",
            ));
        }
        let name = params
            .name
            .unwrap_or_else(|| format!("Preview (requested {})", &now[..now.len().min(10)]));
        validate_plain_line("name", &name, 3, 120)?;
        if let Some(note) = &params.note {
            validate_plain_text("note", note, 1, 500)?;
        }
        self.insert_release(
            repository_id,
            name.trim(),
            ReleaseKind::Preview,
            ReleaseStatus::Requested,
            params.note.as_deref().map(str::trim),
            None,
            actor,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_release(
        &self,
        repository_id: &str,
        name: &str,
        kind: ReleaseKind,
        status: ReleaseStatus,
        note: Option<&str>,
        requested_seq: Option<u32>,
        actor: &str,
        now: &str,
    ) -> Result<ReleaseMutation, ProtocolError> {
        let release_id = ids::release_id().map_err(id_error)?;
        let repository_id_owned = repository_id.to_owned();
        let name = name.to_owned();
        let kind_string = enum_string(&kind)?;
        let status_string = enum_string(&status)?;
        let note = note.map(str::to_owned);
        let requested_at = matches!(&status, ReleaseStatus::Requested).then(|| now.to_owned());
        let actor = actor.to_owned();
        let now = now.to_owned();
        let release_id_for_insert = release_id.clone();
        let transaction_name = name.clone();
        let transaction_note = note.clone();
        let transaction_requested_at = requested_at.clone();
        let seq = self
            .database
            .transaction(move |transaction| {
                let seq = match requested_seq {
                    Some(seq) => seq,
                    None => transaction.query_row(
                        "SELECT COALESCE(MAX(seq),0)+1 FROM releases WHERE repository_id=?1",
                        [&repository_id_owned],
                        |row| row.get(0),
                    )?,
                };
                transaction
                    .execute(
                        "INSERT INTO releases(release_id,repository_id,seq,name,kind,status,note,requested_at,created_at,created_by,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?9)",
                        rusqlite::params![
                            release_id_for_insert,
                            repository_id_owned,
                            seq,
                            transaction_name,
                            kind_string,
                            status_string,
                            transaction_note,
                            transaction_requested_at,
                            now,
                            actor,
                        ],
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::SqliteFailure(_, _) => domain_error(
                            ErrorCode::ParamsInvalid,
                            format!("release sequence {seq} is already in use"),
                        ),
                        other => DatabaseError::Sqlite(other),
                    })?;
                append_plan_event(
                    transaction,
                    &repository_id_owned,
                    "release",
                    &release_id_for_insert,
                    "created",
                    None,
                    Some(&kind_string),
                    &actor,
                    &now,
                    transaction_note.as_deref(),
                )?;
                if status_string == "requested" {
                    append_plan_event(
                        transaction,
                        &repository_id_owned,
                        "release",
                        &release_id_for_insert,
                        "requested",
                        Some("planned"),
                        Some("requested"),
                        &actor,
                        &now,
                        transaction_note.as_deref(),
                    )?;
                }
                Ok(seq)
            })
            .map_err(database_or_domain)?;
        Ok(ReleaseMutation {
            release_id,
            repository_id: Some(repository_id.to_owned()),
            seq,
            name: name.to_owned(),
            kind,
            status,
            note,
            requested_at,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    pub fn update_release(
        &self,
        params: ReleaseUpdate,
        actor: &str,
        now: &str,
    ) -> Result<ReleaseMutation, ProtocolError> {
        let mut release = self.release_row(&params.release_id)?.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ReleaseNotFound,
                format!("no release {}", params.release_id),
            )
        })?;
        self.require_active_repository(&release.repository_id)?;
        let old = release.clone();
        let mut edited = Vec::new();
        let mut status_event = None;
        if let Some(name) = params.name {
            validate_plain_line("name", &name, 3, 120)?;
            let name = name.trim().to_owned();
            if name != release.name {
                release.name = name;
                edited.push("name");
            }
        }
        if let Some(note) = params.note {
            validate_plain_text("note", &note, 1, 500)?;
            let note = note.trim().to_owned();
            if release.note.as_deref() != Some(&note) {
                release.note = Some(note);
                edited.push("note");
            }
        }
        if let Some(seq) = params.seq
            && seq != release.seq
        {
            release.seq = seq;
            edited.push("seq");
        }
        if let Some(status) = params.status {
            let status = enum_string(&status)?;
            if status != release.status {
                let allowed = matches!(
                    (release.status.as_str(), status.as_str()),
                    ("planned", "dropped") | ("dropped", "planned")
                );
                if !allowed {
                    return Err(ProtocolError::new(
                        ErrorCode::ParamsInvalid,
                        "status may only change between planned and dropped here",
                    ));
                }
                status_event = Some((release.status.clone(), status.clone()));
                release.status = status;
            }
        }
        if release.seq == old.seq
            && release.name == old.name
            && release.note == old.note
            && release.status == old.status
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "nothing to change",
            ));
        }
        let transaction_release = release.clone();
        let actor = actor.to_owned();
        let now = now.to_owned();
        self.database
            .transaction(move |transaction| {
                transaction
                    .execute(
                        "UPDATE releases SET seq=?1,name=?2,note=?3,status=?4,updated_at=?5 WHERE release_id=?6",
                        rusqlite::params![
                            transaction_release.seq,
                            transaction_release.name,
                            transaction_release.note,
                            transaction_release.status,
                            now,
                            transaction_release.release_id,
                        ],
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::SqliteFailure(_, _) => domain_error(
                            ErrorCode::ParamsInvalid,
                            format!(
                                "release sequence {} is already in use",
                                transaction_release.seq
                            ),
                        ),
                        other => DatabaseError::Sqlite(other),
                    })?;
                if let Some((from, to)) = status_event {
                    append_plan_event(
                        transaction,
                        &transaction_release.repository_id,
                        "release",
                        &transaction_release.release_id,
                        "status",
                        Some(&from),
                        Some(&to),
                        &actor,
                        &now,
                        None,
                    )?;
                }
                if !edited.is_empty() {
                    edited.sort_unstable();
                    append_plan_event(
                        transaction,
                        &transaction_release.repository_id,
                        "release",
                        &transaction_release.release_id,
                        "edited",
                        None,
                        Some(&edited.join(",")),
                        &actor,
                        &now,
                        None,
                    )?;
                }
                Ok(())
            })
            .map_err(database_or_domain)?;
        release_mutation(release, self.elaboration_requests(&old.repository_id)?)
    }

    pub fn deliver_release(
        &self,
        params: ReleaseDeliver,
        actor: &str,
        now: &str,
    ) -> Result<ReleaseDelivered, ProtocolError> {
        let release_id = params.release_id.clone();
        let release = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT repository_id,name,status FROM releases WHERE release_id=?1",
                        [&release_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::ReleaseNotFound,
                    format!("no release {}", params.release_id),
                )
            })?;
        let (repository_id, release_name, prior_status) = release;
        self.require_active_repository(&repository_id)?;
        if !matches!(prior_status.as_str(), "planned" | "requested") {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!(
                    "release {release_name:?} is already {prior_status}; deliver a new release"
                ),
            ));
        }
        let evidence = self.deployments.read(&params.deployment_id)?;
        if evidence.repository_id != repository_id {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "the deployment belongs to another repository",
            ));
        }
        let event_value = serde_json::to_string(&serde_json::json!({
            "deployment_id": evidence.deployment_id,
            "generation_number": evidence.generation_number,
            "commit_hash": evidence.commit_hash,
            "dirty": evidence.dirty,
            "url": evidence.url,
            "port": evidence.port,
        }))
        .map_err(|error| {
            ProtocolError::new(ErrorCode::InternalError, "cannot encode delivery evidence")
                .with_detail(error.to_string())
        })?;
        let update_release_id = params.release_id.clone();
        let update_repository_id = repository_id.clone();
        let actor = actor.to_owned();
        let note = params.note.clone();
        let now = now.to_owned();
        let transaction_now = now.clone();
        let evidence_for_update = evidence.clone();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE releases SET status='delivered',delivered_at=?1,deployment_id=?2,generation_number=?3,commit_hash=?4,dirty=?5,fingerprint=?6,url=?7,port=?8,updated_at=?1 WHERE release_id=?9",
                    rusqlite::params![
                        transaction_now,
                        evidence_for_update.deployment_id,
                        evidence_for_update.generation_number,
                        evidence_for_update.commit_hash,
                        i64::from(evidence_for_update.dirty),
                        evidence_for_update.fingerprint,
                        evidence_for_update.url,
                        evidence_for_update.port,
                        update_release_id,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,from_value,to_value,actor,at,note) VALUES(?1,'release',?2,'delivered',?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        update_repository_id,
                        update_release_id,
                        prior_status,
                        event_value,
                        actor,
                        transaction_now,
                        note,
                    ],
                )?;
                Ok(())
            })
            .map_err(database_or_domain)?;
        Ok(ReleaseDelivered {
            release_id: params.release_id,
            status: ReleaseStatus::Delivered,
            delivered_at: now,
            url: evidence.url,
            port: Some(evidence.port),
            commit_hash: evidence.commit_hash,
            dirty: evidence.dirty,
            generation_number: evidence.generation_number,
            elaboration_requests: self.elaboration_requests(&repository_id)?,
        })
    }

    pub fn record_decision(
        &self,
        repository_id: &str,
        params: DecisionRecord,
        actor: &str,
        now: &str,
    ) -> Result<DecisionRecorded, ProtocolError> {
        self.require_active_repository(repository_id)?;
        validate_plain_line("title", &params.title, 3, 120)?;
        validate_plain_text("body", &params.body, 10, 4000)?;
        if let Some(value) = &params.technical_note {
            validate_plain_text("technical_note", value, 1, 4000)?;
        }
        if let Some(value) = &params.r#ref
            && (value.len() < 3
                || value.len() > 80
                || value.chars().any(|character| character.is_whitespace()))
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "ref must be a stable key without whitespace (3..80 characters)",
            ));
        }
        let superseded = if let Some(reference) = &params.supersedes {
            Some(self.decision_for_supersession(repository_id, reference)?)
        } else {
            None
        };
        let decision_id = ids::decision_id().map_err(id_error)?;
        let repository_id_owned = repository_id.to_owned();
        let actor = actor.to_owned();
        let now = now.to_owned();
        let aspect = enum_string(&params.aspect)?;
        let decision_id_for_insert = decision_id.clone();
        let reference = params.r#ref.clone();
        let title = params.title.trim().to_owned();
        let body = params.body.trim().to_owned();
        let technical_note = params
            .technical_note
            .as_deref()
            .map(str::trim)
            .map(str::to_owned);
        let seq = self
            .database
            .transaction(move |transaction| {
                let seq: u32 = transaction.query_row(
                    "SELECT COALESCE(MAX(seq),0)+1 FROM decisions WHERE repository_id=?1",
                    [&repository_id_owned],
                    |row| row.get(0),
                )?;
                transaction
                    .execute(
                        "INSERT INTO decisions(decision_id,repository_id,seq,ref,aspect,title,body,technical_note,created_at,created_by) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                        rusqlite::params![
                            decision_id_for_insert,
                            repository_id_owned,
                            seq,
                            reference,
                            aspect,
                            title,
                            body,
                            technical_note,
                            now,
                            actor,
                        ],
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::SqliteFailure(_, _) => domain_error(
                            ErrorCode::ParamsInvalid,
                            "that decision ref is already used in this repository",
                        ),
                        other => DatabaseError::Sqlite(other),
                    })?;
                if let Some((superseded_id, _)) = superseded {
                    transaction.execute(
                        "UPDATE decisions SET superseded_by=?1 WHERE decision_id=?2 AND superseded_by IS NULL",
                        rusqlite::params![decision_id_for_insert, superseded_id],
                    )?;
                }
                Ok(seq)
            })
            .map_err(database_or_domain)?;
        let unsummarized_count = self.unsummarized_count(repository_id)?;
        Ok(DecisionRecorded {
            decision_id,
            seq,
            r#ref: params.r#ref,
            unsummarized_count,
            summary_due: unsummarized_count >= SUMMARY_DUE_THRESHOLD,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    pub fn decision_tail(
        &self,
        repository_id: &str,
        params: DecisionTailParams,
    ) -> Result<DecisionTail, ProtocolError> {
        let (display_name, _) = self.repository_identity(repository_id)?;
        let aspect = params.aspect.as_ref().map(enum_string).transpose()?;
        let repository_id_owned = repository_id.to_owned();
        let limit = u32::from(params.n.clamp(1, 50));
        let before = params.before_seq;
        let (decisions, has_more, summary) = self
            .database
            .call(move |connection| {
                let mut sql = "SELECT decision_id,seq,ref,aspect,title,body,technical_note,superseded_by,created_at,created_by FROM decisions WHERE repository_id=?1".to_owned();
                let mut values: Vec<rusqlite::types::Value> =
                    vec![repository_id_owned.clone().into()];
                if let Some(aspect) = aspect {
                    sql.push_str(" AND aspect=?2");
                    values.push(aspect.into());
                }
                if let Some(before) = before {
                    sql.push_str(&format!(" AND seq<?{}", values.len() + 1));
                    values.push(i64::from(before).into());
                }
                sql.push_str(&format!(" ORDER BY seq DESC LIMIT ?{}", values.len() + 1));
                values.push(i64::from(limit + 1).into());
                let mut statement = connection.prepare(&sql)?;
                let rows = statement
                    .query_map(rusqlite::params_from_iter(values), decision_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                let has_more = rows.len() > limit as usize;
                let decisions = rows.into_iter().take(limit as usize).rev().collect();
                let summary = connection
                    .query_row(
                        "SELECT body,covers_through_seq,created_at FROM decision_summaries WHERE repository_id=?1 ORDER BY covers_through_seq DESC LIMIT 1",
                        [&repository_id_owned],
                        |row| {
                            Ok(DecisionSummary {
                                body: row.get(0)?,
                                covers_through_seq: row.get(1)?,
                                created_at: row.get(2)?,
                            })
                        },
                    )
                    .optional()?;
                Ok((decisions, has_more, summary))
            })
            .map_err(database_or_domain)?;
        let unsummarized_count = self.unsummarized_count(repository_id)?;
        Ok(DecisionTail {
            repository_id: repository_id.to_owned(),
            display_name,
            summary,
            decisions,
            has_more,
            unsummarized_count,
            summary_due: unsummarized_count >= SUMMARY_DUE_THRESHOLD,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    pub fn search_decisions(
        &self,
        repository_id: &str,
        params: DecisionSearchParams,
    ) -> Result<DecisionSearch, ProtocolError> {
        self.repository_identity(repository_id)?;
        validate_plain_text("query", &params.query, 1, 200)?;
        let query = literal_fts_query(&params.query);
        let aspect = params.aspect.as_ref().map(enum_string).transpose()?;
        let repository_id_owned = repository_id.to_owned();
        let limit = u32::from(params.n.clamp(1, 50));
        let decisions = self
            .database
            .call(move |connection| {
                let mut sql = "SELECT d.decision_id,d.seq,d.ref,d.aspect,d.title,d.body,d.technical_note,d.superseded_by,d.created_at,d.created_by FROM decisions_fts f JOIN decisions d ON d.rowid=f.rowid WHERE decisions_fts MATCH ?1 AND d.repository_id=?2".to_owned();
                let mut values: Vec<rusqlite::types::Value> =
                    vec![query.into(), repository_id_owned.into()];
                if let Some(aspect) = aspect {
                    sql.push_str(" AND d.aspect=?3");
                    values.push(aspect.into());
                }
                sql.push_str(&format!(" ORDER BY bm25(decisions_fts) LIMIT ?{}", values.len() + 1));
                values.push(i64::from(limit + 1).into());
                let mut statement = connection.prepare(&sql)?;
                Ok(statement
                    .query_map(rusqlite::params_from_iter(values), decision_from_row)?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_or_domain)?;
        let has_more = decisions.len() > limit as usize;
        Ok(DecisionSearch {
            repository_id: repository_id.to_owned(),
            query: params.query.trim().to_owned(),
            decisions: decisions.into_iter().take(limit as usize).collect(),
            has_more,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    pub fn summarize_decisions(
        &self,
        repository_id: &str,
        params: DecisionSummarize,
        actor: &str,
        now: &str,
    ) -> Result<DecisionSummarized, ProtocolError> {
        self.require_active_repository(repository_id)?;
        validate_plain_text("body", &params.body, 10, 16_000)?;
        let repository_id_owned = repository_id.to_owned();
        let (max_seq, covered): (u32, u32) = self
            .database
            .call(move |connection| {
                Ok((
                    connection.query_row(
                        "SELECT COALESCE(MAX(seq),0) FROM decisions WHERE repository_id=?1",
                        [&repository_id_owned],
                        |row| row.get(0),
                    )?,
                    connection.query_row(
                        "SELECT COALESCE(MAX(covers_through_seq),0) FROM decision_summaries WHERE repository_id=?1",
                        [&repository_id_owned],
                        |row| row.get(0),
                    )?,
                ))
            })
            .map_err(database_or_domain)?;
        if params.covers_through_seq > max_seq {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!(
                    "covers_through_seq {} is beyond the latest decision ({max_seq})",
                    params.covers_through_seq
                ),
            ));
        }
        if params.covers_through_seq <= covered {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("decisions through {covered} are already summarized"),
            ));
        }
        let repository_id_owned = repository_id.to_owned();
        let body = params.body.trim().to_owned();
        let covers = params.covers_through_seq;
        let actor = actor.to_owned();
        let now = now.to_owned();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO decision_summaries(repository_id,covers_through_seq,body,created_at,created_by) VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![repository_id_owned, covers, body, now, actor],
                )?;
                Ok(())
            })
            .map_err(database_or_domain)?;
        let unsummarized_count = self.unsummarized_count(repository_id)?;
        Ok(DecisionSummarized {
            repository_id: repository_id.to_owned(),
            covers_through_seq: covers,
            unsummarized_count,
            summary_due: unsummarized_count >= SUMMARY_DUE_THRESHOLD,
            elaboration_requests: self.elaboration_requests(repository_id)?,
        })
    }

    fn decision_for_supersession(
        &self,
        repository_id: &str,
        reference: &str,
    ) -> Result<(String, Option<String>), ProtocolError> {
        let repository_id = repository_id.to_owned();
        let reference = reference.to_owned();
        let decision = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT decision_id,superseded_by FROM decisions WHERE repository_id=?1 AND (decision_id=?2 OR ref=?2) LIMIT 1",
                        rusqlite::params![repository_id, reference],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)?
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::DecisionNotFound, "decision was not found")
            })?;
        if let Some(successor) = &decision.1 {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("that decision is already superseded by {successor}"),
            ));
        }
        Ok(decision)
    }

    fn task_row(&self, task_id: &str) -> Result<Option<TaskRow>, ProtocolError> {
        let task_id = task_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at FROM tasks WHERE task_id=?1",
                        [&task_id],
                        task_from_row,
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)
    }

    fn release_row(&self, release_id: &str) -> Result<Option<ReleaseRow>, ProtocolError> {
        let release_id = release_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT release_id,repository_id,seq,name,kind,status,note,requested_at FROM releases WHERE release_id=?1",
                        [&release_id],
                        |row| {
                            Ok(ReleaseRow {
                                release_id: row.get(0)?,
                                repository_id: row.get(1)?,
                                seq: row.get(2)?,
                                name: row.get(3)?,
                                kind: row.get(4)?,
                                status: row.get(5)?,
                                note: row.get(6)?,
                                requested_at: row.get(7)?,
                            })
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)
    }

    fn require_open_release(
        &self,
        repository_id: &str,
        release_id: &str,
    ) -> Result<(), ProtocolError> {
        let repository_id = repository_id.to_owned();
        let release_id = release_id.to_owned();
        let row = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT repository_id,status FROM releases WHERE release_id=?1",
                        [&release_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)?
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::ReleaseNotFound, "release was not found")
            })?;
        if row.0 != repository_id {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "the release belongs to another repository",
            ));
        }
        if matches!(row.1.as_str(), "delivered" | "dropped") {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "tasks can only be planned into an open release",
            ));
        }
        Ok(())
    }

    fn would_create_cycle(
        &self,
        task_id: &str,
        candidate_parent: &str,
    ) -> Result<bool, ProtocolError> {
        let task_id = task_id.to_owned();
        let candidate_parent = candidate_parent.to_owned();
        self.database
            .call(move |connection| {
                Ok(connection.query_row(
                    "WITH RECURSIVE descendants(task_id) AS (SELECT ?1 UNION ALL SELECT tasks.task_id FROM tasks JOIN descendants ON tasks.parent_task_id=descendants.task_id) SELECT EXISTS(SELECT 1 FROM descendants WHERE task_id=?2)",
                    rusqlite::params![task_id, candidate_parent],
                    |row| row.get::<_, i64>(0),
                )? != 0)
            })
            .map_err(database_or_domain)
    }

    fn has_requested(&self, repository_id: &str) -> Result<bool, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM releases WHERE repository_id=?1 AND status='requested')",
                    [&repository_id],
                    |row| row.get::<_, i64>(0),
                )? != 0)
            })
            .map_err(database_or_domain)
    }

    fn preview_requests(&self, repository_id: &str) -> Result<Vec<PreviewRequest>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT release_id,name,requested_at,note FROM releases WHERE repository_id=?1 AND status='requested' ORDER BY seq",
                )?;
                Ok(statement
                    .query_map([repository_id], |row| {
                        Ok(PreviewRequest {
                            release_id: row.get(0)?,
                            name: row.get(1)?,
                            requested_at: row.get(2)?,
                            note: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_or_domain)
    }

    fn repository_identity(&self, repository_id: &str) -> Result<(String, bool), ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT display_name,archived_at IS NOT NULL FROM repositories WHERE repository_id=?1",
                        [&repository_id],
                        |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)?
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::RepositoryNotFound, "repository is not registered")
            })
    }

    fn unsummarized_count(&self, repository_id: &str) -> Result<u32, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM decisions WHERE repository_id=?1 AND seq>(SELECT COALESCE(MAX(covers_through_seq),0) FROM decision_summaries WHERE repository_id=?1)",
                    [&repository_id],
                    |row| row.get(0),
                )?)
            })
            .map_err(database_or_domain)
    }

    fn require_active_repository(&self, repository_id: &str) -> Result<(), ProtocolError> {
        let repository_id = repository_id.to_owned();
        let archived = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT archived_at FROM repositories WHERE repository_id=?1",
                        [&repository_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_or_domain)?;
        match archived {
            None => Err(ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                "repository is not registered",
            )),
            Some(Some(_)) => Err(ProtocolError::new(
                ErrorCode::RepositoryArchived,
                "repository is archived",
            )),
            Some(None) => Ok(()),
        }
    }

    fn elaboration_requests(
        &self,
        repository_id: &str,
    ) -> Result<Vec<ElaborationRequest>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT task_id,title,outcome,status,kind,(SELECT at FROM plan_events WHERE subject_kind='task' AND subject_id=tasks.task_id AND event='elaboration_requested' ORDER BY event_id DESC LIMIT 1) FROM tasks WHERE repository_id=?1 AND elaboration_needed=1 ORDER BY seq",
                )?;
                let rows = statement.query_map([repository_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })?;
                let mut result = Vec::new();
                for row in rows {
                    let (task_id, title, outcome, status, kind, requested_at) = row?;
                    result.push(ElaborationRequest {
                        task_id,
                        title,
                        outcome,
                        status: parse_task_status(&status)?,
                        kind: parse_task_kind(&kind)?,
                        requested_at,
                    });
                }
                Ok(result)
            })
            .map_err(database_or_domain)
    }
}

fn decision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Decision> {
    let aspect: String = row.get(3)?;
    let aspect = serde_json::from_value(serde_json::Value::String(aspect)).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Decision {
        decision_id: row.get(0)?,
        seq: row.get(1)?,
        r#ref: row.get(2)?,
        aspect,
        title: row.get(4)?,
        body: row.get(5)?,
        technical_note: row.get(6)?,
        superseded_by: row.get(7)?,
        created_at: row.get(8)?,
        created_by: row.get(9)?,
    })
}

fn read_tasks(
    connection: &rusqlite::Connection,
    repository_id: &str,
    _collection_projection: bool,
) -> Result<Vec<TaskRow>, DatabaseError> {
    let mut statement = connection.prepare(
        "SELECT task_id,repository_id,parent_task_id,release_id,seq,position,title,outcome,impact,unblock_condition,verification,technical_note,kind,status,estimated_loc,elaboration_needed,created_at,created_by,updated_at FROM tasks WHERE repository_id=?1 AND status!='dropped' ORDER BY seq",
    )?;
    Ok(statement
        .query_map([repository_id], task_from_row)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn read_release_overviews(
    connection: &rusqlite::Connection,
    repository_id: &str,
) -> Result<Vec<ReleaseOverviewRow>, DatabaseError> {
    let mut statement = connection.prepare(
        "SELECT release_id,seq,name,kind,status,note,requested_at,delivered_at,url,port FROM releases WHERE repository_id=?1 AND status!='dropped' ORDER BY seq",
    )?;
    Ok(statement
        .query_map([repository_id], |row| {
            Ok(ReleaseOverviewRow {
                release_id: row.get(0)?,
                seq: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                status: row.get(4)?,
                note: row.get(5)?,
                requested_at: row.get(6)?,
                delivered_at: row.get(7)?,
                url: row.get(8)?,
                port: row.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn leaf_aggregates(tasks: &[TaskRow]) -> (BTreeMap<Option<String>, Aggregate>, HashSet<String>) {
    let parents = tasks
        .iter()
        .filter_map(|task| task.parent_task_id.clone())
        .collect::<HashSet<_>>();
    let mut aggregates = BTreeMap::new();
    for task in tasks {
        if parents.contains(&task.task_id) {
            continue;
        }
        let aggregate = aggregates
            .entry(task.release_id.clone())
            .or_insert_with(Aggregate::default);
        let lines = u64::from(task.estimated_loc.unwrap_or(0));
        aggregate.tasks_total += 1;
        aggregate.loc_total += lines;
        if task.status == "done" {
            aggregate.tasks_done += 1;
            aggregate.loc_done += lines;
        }
    }
    (aggregates, parents)
}

fn clip(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut clipped = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    while clipped.chars().last().is_some_and(char::is_whitespace) {
        clipped.pop();
    }
    clipped.push('…');
    clipped
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        task_id: row.get(0)?,
        repository_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        release_id: row.get(3)?,
        seq: row.get(4)?,
        position: row.get(5)?,
        title: row.get(6)?,
        outcome: row.get(7)?,
        impact: row.get(8)?,
        unblock_condition: row.get(9)?,
        verification: row.get(10)?,
        technical_note: row.get(11)?,
        kind: row.get(12)?,
        status: row.get(13)?,
        estimated_loc: row.get(14)?,
        elaboration_needed: row.get::<_, i64>(15)? != 0,
        created_at: row.get(16)?,
        created_by: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn task_result(row: TaskRow) -> Result<Task, ProtocolError> {
    Ok(Task {
        task_id: row.task_id,
        repository_id: row.repository_id,
        parent_task_id: row.parent_task_id,
        release_id: row.release_id,
        seq: row.seq,
        position: row.position,
        title: row.title,
        outcome: row.outcome,
        impact: row.impact,
        unblock_condition: row.unblock_condition,
        verification: row.verification,
        technical_note: row.technical_note,
        kind: parse_task_kind(&row.kind).map_err(database_or_domain)?,
        status: parse_task_status(&row.status).map_err(database_or_domain)?,
        estimated_loc: row.estimated_loc,
        elaboration_needed: row.elaboration_needed,
        created_at: row.created_at,
        created_by: row.created_by,
        updated_at: row.updated_at,
    })
}

fn task_mutation(
    row: TaskRow,
    preview_requested: bool,
    elaboration_requests: Vec<ElaborationRequest>,
) -> Result<TaskMutation, ProtocolError> {
    Ok(TaskMutation {
        task_id: row.task_id,
        repository_id: row.repository_id,
        seq: row.seq,
        position: row.position,
        title: row.title,
        status: parse_task_status(&row.status).map_err(database_or_domain)?,
        kind: parse_task_kind(&row.kind).map_err(database_or_domain)?,
        release_id: row.release_id,
        parent_task_id: row.parent_task_id,
        estimated_loc: row.estimated_loc,
        elaboration_needed: row.elaboration_needed,
        preview_requested,
        elaboration_requests,
    })
}

fn release_mutation(
    row: ReleaseRow,
    elaboration_requests: Vec<ElaborationRequest>,
) -> Result<ReleaseMutation, ProtocolError> {
    Ok(ReleaseMutation {
        release_id: row.release_id,
        repository_id: Some(row.repository_id),
        seq: row.seq,
        name: row.name,
        kind: parse_release_kind(&row.kind).map_err(database_or_domain)?,
        status: parse_release_status(&row.status).map_err(database_or_domain)?,
        note: row.note,
        requested_at: row.requested_at,
        elaboration_requests,
    })
}

fn place_task(
    transaction: &rusqlite::Transaction<'_>,
    repository_id: &str,
    parent_task_id: Option<&str>,
    release_id: Option<&str>,
    task_id: &str,
    index: Option<u32>,
) -> Result<u32, DatabaseError> {
    let mut statement = transaction.prepare(
        "SELECT task_id FROM tasks WHERE repository_id=?1 AND parent_task_id IS ?2 AND release_id IS ?3 AND status!='dropped' AND task_id!=?4 ORDER BY position,seq",
    )?;
    let mut siblings = statement
        .query_map(
            rusqlite::params![repository_id, parent_task_id, release_id, task_id],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let index = index
        .unwrap_or(siblings.len() as u32)
        .min(siblings.len() as u32) as usize;
    siblings.insert(index, task_id.to_owned());
    let mut position = 0;
    for (offset, sibling) in siblings.into_iter().enumerate() {
        let next = (offset + 1) as u32;
        transaction.execute(
            "UPDATE tasks SET position=?1 WHERE task_id=?2",
            rusqlite::params![next, sibling],
        )?;
        if sibling == task_id {
            position = next;
        }
    }
    Ok(position)
}

#[allow(clippy::too_many_arguments)]
fn append_plan_event(
    transaction: &rusqlite::Transaction<'_>,
    repository_id: &str,
    subject_kind: &str,
    subject_id: &str,
    event: &str,
    from_value: Option<&str>,
    to_value: Option<&str>,
    actor: &str,
    now: &str,
    note: Option<&str>,
) -> Result<(), DatabaseError> {
    transaction.execute(
        "INSERT INTO plan_events(repository_id,subject_kind,subject_id,event,from_value,to_value,actor,at,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![
            repository_id,
            subject_kind,
            subject_id,
            event,
            from_value,
            to_value,
            actor,
            now,
            note,
        ],
    )?;
    Ok(())
}

fn literal_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn enum_string<T: serde::Serialize>(value: &T) -> Result<String, ProtocolError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| ProtocolError::new(ErrorCode::InternalError, "cannot encode enum value"))
}

fn validate_plain_line(
    name: &str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), ProtocolError> {
    let trimmed = value.trim();
    if trimmed.len() < minimum || trimmed.len() > maximum || value.contains(['\r', '\n']) {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("{name} must be one plain line of {minimum}..{maximum} characters"),
        ));
    }
    Ok(())
}

fn validate_plain_text(
    name: &str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), ProtocolError> {
    let length = value.trim().len();
    if length < minimum || length > maximum {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("{name} must contain {minimum}..{maximum} characters"),
        ));
    }
    Ok(())
}

fn id_error(error: ids::IdError) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, "cannot create an identifier")
        .with_detail(error.to_string())
}

fn parse_task_status(value: &str) -> Result<TaskStatus, DatabaseError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(ErrorCode::InternalError, "stored task status is invalid")
                .with_detail(error.to_string()),
        )
    })
}

fn parse_task_kind(value: &str) -> Result<TaskKind, DatabaseError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(ErrorCode::InternalError, "stored task kind is invalid")
                .with_detail(error.to_string()),
        )
    })
}

fn parse_release_kind(value: &str) -> Result<ReleaseKind, DatabaseError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(ErrorCode::InternalError, "stored release kind is invalid")
                .with_detail(error.to_string()),
        )
    })
}

fn parse_release_status(value: &str) -> Result<ReleaseStatus, DatabaseError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(ErrorCode::InternalError, "stored release status is invalid")
                .with_detail(error.to_string()),
        )
    })
}

fn domain_error(code: ErrorCode, message: impl Into<String>) -> DatabaseError {
    DatabaseError::Domain(ProtocolError::new(code, message))
}

fn database_or_domain(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(ErrorCode::InternalError, "database operation failed")
            .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devcoordinator2_api::params::DecisionAspect;
    use tempfile::tempdir;

    fn world() -> (tempfile::TempDir, Database, PlanService) {
        let temporary = tempdir().expect("tempdir");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        let evidence = Arc::new(SqliteDeploymentEvidence::new(
            database.clone(),
            "example.test",
        ));
        let service = PlanService::new(database.clone(), evidence);
        (temporary, database, service)
    }

    #[test]
    fn repository_index_resolves_roots_and_local_clones_without_test_results() {
        let (temporary, database, service) = world();
        let registry = crate::repository::Registry::new(database);
        let root = temporary.path().join("project");
        let clone = temporary.path().join("workspace");
        for path in [&root, &clone] {
            std::fs::create_dir(path).unwrap();
            assert!(
                std::process::Command::new("git")
                    .arg("init")
                    .arg(path)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        for (path, origin) in [
            (&root, "https://example.test/owner/project.git".to_owned()),
            (&clone, root.to_string_lossy().into_owned()),
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(path)
                    .args(["remote", "add", "origin", &origin])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            registry
                .register(path, unsafe { libc::geteuid() }, unsafe { libc::getegid() })
                .unwrap();
        }
        let PlanOverview::Collection(collection) = service.overview(None).unwrap() else {
            panic!("collection expected")
        };
        assert_eq!(collection.repositories.len(), 2);
        let source = collection.repositories[0]
            .repository_source
            .as_ref()
            .unwrap();
        assert_eq!(source.name, "project");
        for row in &collection.repositories {
            assert_eq!(row.repository_source.as_ref(), Some(source));
            assert_eq!(row.open_tasks, 0);
            assert!(
                row.root_path == root.to_string_lossy() || row.root_path == clone.to_string_lossy()
            );
        }
        std::fs::remove_dir_all(&clone).unwrap();
        let PlanOverview::Collection(collection) = service.overview(None).unwrap() else {
            panic!("collection expected")
        };
        let missing = collection
            .repositories
            .iter()
            .find(|row| row.display_name == "workspace")
            .unwrap();
        assert_eq!(missing.root_path, clone.to_string_lossy());
        assert!(missing.repository_source.is_none());
    }

    #[test]
    fn repository_presentation_persists_without_renaming_repository_identity() {
        let (temporary, database, service) = world();
        seed_repository(&database);
        let registry = crate::repository::Registry::new(database.clone());
        let request = devcoordinator2_api::params::RepositoryPresentationUpdate {
            repository_id: "r1111111111111111".to_owned(),
            display_name: Some("  My project  ".to_owned()),
            icon: Some("rocket".to_owned()),
        };
        registry.update_presentation(request.clone(), 1000).unwrap();
        let PlanOverview::Collection(collection) = service.overview(None).unwrap() else {
            panic!("collection expected")
        };
        let row = &collection.repositories[0];
        assert_eq!(row.display_name, "Fixture repository");
        assert_eq!(row.repository_id, request.repository_id);
        assert_eq!(
            row.presentation.as_ref().unwrap().display_name.as_deref(),
            Some("My project")
        );
        assert_eq!(
            row.presentation.as_ref().unwrap().icon.as_deref(),
            Some("rocket")
        );
        let reopened = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        let persisted: (String, String, String) = reopened.call(|connection| {
            Ok(connection.query_row("SELECT r.root_path,r.display_name,p.display_name FROM repositories r JOIN repository_presentation p USING(repository_id)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?)
        }).unwrap();
        assert_eq!(
            persisted,
            (
                "/fixture/repository".into(),
                "Fixture repository".into(),
                "My project".into()
            )
        );
        for name in ["", "   ", "bad\nname", &"x".repeat(81)] {
            let mut invalid = request.clone();
            invalid.display_name = Some(name.into());
            assert_eq!(
                registry
                    .update_presentation(invalid, 1000)
                    .unwrap_err()
                    .code,
                ErrorCode::ParamsInvalid
            );
        }
        let mut invalid = request.clone();
        invalid.icon = Some("../private".into());
        assert_eq!(
            registry
                .update_presentation(invalid, 1000)
                .unwrap_err()
                .code,
            ErrorCode::ParamsInvalid
        );
        let mut missing = request.clone();
        missing.repository_id = "r2222222222222222".into();
        assert_eq!(
            registry
                .update_presentation(missing, 1000)
                .unwrap_err()
                .code,
            ErrorCode::RepositoryNotFound
        );
        registry
            .update_presentation(
                devcoordinator2_api::params::RepositoryPresentationUpdate {
                    repository_id: request.repository_id,
                    display_name: None,
                    icon: None,
                },
                1000,
            )
            .unwrap();
        let PlanOverview::Collection(collection) = service.overview(None).unwrap() else {
            panic!("collection expected")
        };
        assert!(collection.repositories[0].presentation.is_none());
    }

    fn seed(database: &Database, spec_json: String) {
        database
            .transaction(move |transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/fixture/repository','Fixture repository','t',1000,'t')", [])?;
                transaction.execute("INSERT INTO worktrees VALUES('w1111111111111111','r1111111111111111','/fixture/repository','t','t')", [])?;
                transaction.execute(
                    "INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,created_at,created_by_uid,client,updated_at,public) VALUES('d1111111111111111','r1111111111111111','w1111111111111111','fixture','checkout',NULL,'spec',?1,'running',1,'t',1000,'codex','t',0)",
                    [spec_json],
                )?;
                transaction.execute("INSERT INTO generations VALUES('d1111111111111111',1,'0123456789abcdef',0,'/fixture/generation','fingerprint','t','current')", [])?;
                transaction.execute("INSERT INTO port_assignments VALUES(24001,'d1111111111111111','api',1,'t')", [])?;
                transaction.execute("INSERT INTO port_assignments VALUES(24002,'d1111111111111111','web',1,'t')", [])?;
                transaction.execute("INSERT INTO releases(release_id,repository_id,seq,name,kind,status,created_at,created_by,updated_at) VALUES('v1111111111111111','r1111111111111111',1,'Fixture release','release','planned','t','uid:1000','t')", [])?;
                Ok(())
            })
            .expect("seed");
    }

    fn seed_repository(database: &Database) {
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/fixture/repository','Fixture repository','t',1000,'t')", [])?;
                Ok(())
            })
            .expect("seed repository");
    }

    fn task_params(title: &str) -> TaskCreate {
        TaskCreate {
            path: None,
            repository_id: Some("r1111111111111111".into()),
            title: title.into(),
            kind: TaskKind::Improvement,
            outcome: Some(format!("{title} works for the owner.")),
            impact: None,
            unblock_condition: None,
            verification: None,
            technical_note: None,
            parent_task_id: None,
            release_id: None,
            estimated_loc: Some(100),
        }
    }

    #[test]
    fn no_domain_delivery_uses_the_declared_route_component() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../contracts/fixtures/no-domain-release-route-v2.json"
        ))
        .expect("fixture");
        let (_temporary, database, service) = world();
        seed(
            &database,
            serde_json::json!({
                "components": [
                    {"name":"api","type":"process","wants_port":true,"route":false},
                    {"name":"web","type":"process","wants_port":true,"route":true}
                ]
            })
            .to_string(),
        );
        let delivered = service
            .deliver_release(
                ReleaseDeliver {
                    release_id: "v1111111111111111".into(),
                    deployment_id: "d1111111111111111".into(),
                    note: None,
                },
                "uid:1000",
                "2026-09-03T12:00:00Z",
            )
            .expect("deliver");
        assert_eq!(
            delivered.port,
            fixture["expected"]["local_success"]["data"]["port"]
                .as_u64()
                .map(|port| port as u16)
        );
        let stored: u16 = database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT port FROM releases WHERE release_id='v1111111111111111'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .expect("stored port");
        assert_eq!(stored, 24002);
    }

    #[test]
    fn no_domain_delivery_accepts_one_implicit_route_without_using_unrelated_ports() {
        let (_temporary, database, service) = world();
        seed(
            &database,
            serde_json::json!({
                "components": [{"name":"web","type":"process","wants_port":true,"route":false}]
            })
            .to_string(),
        );
        let delivered = service
            .deliver_release(
                ReleaseDeliver {
                    release_id: "v1111111111111111".into(),
                    deployment_id: "d1111111111111111".into(),
                    note: None,
                },
                "uid:1000",
                "2026-09-05T12:00:00Z",
            )
            .expect("implicit route");
        assert_eq!(delivered.port, Some(24002));
        assert_eq!(delivered.url, None);
    }

    #[test]
    fn no_domain_delivery_with_missing_routed_port_preserves_release_and_history() {
        let (_temporary, database, service) = world();
        seed(
            &database,
            serde_json::json!({
                "components": [
                    {"name":"api","type":"process","wants_port":true,"route":false},
                    {"name":"web","type":"process","wants_port":true,"route":true}
                ]
            })
            .to_string(),
        );
        database
            .transaction(|transaction| {
                transaction.execute("DELETE FROM port_assignments WHERE component='web'", [])?;
                Ok(())
            })
            .expect("remove only fixture route port");
        assert!(
            service
                .deliver_release(
                    ReleaseDeliver {
                        release_id: "v1111111111111111".into(),
                        deployment_id: "d1111111111111111".into(),
                        note: None,
                    },
                    "uid:1000",
                    "2026-09-05T12:00:00Z"
                )
                .is_err()
        );
        let state: (String, i64) = database
            .call(|connection| {
                Ok((
                    connection.query_row(
                        "SELECT status FROM releases WHERE release_id='v1111111111111111'",
                        [],
                        |row| row.get(0),
                    )?,
                    connection
                        .query_row("SELECT count(*) FROM plan_events", [], |row| row.get(0))?,
                ))
            })
            .expect("unchanged fixture state");
        assert_eq!(state, ("planned".into(), 0));
    }

    #[test]
    fn ambiguous_route_does_not_mutate_the_release_or_history() {
        let (_temporary, database, service) = world();
        seed(
            &database,
            serde_json::json!({
                "components": [
                    {"name":"api","type":"process","wants_port":true,"route":false},
                    {"name":"web","type":"process","wants_port":true,"route":false}
                ]
            })
            .to_string(),
        );
        let error = service
            .deliver_release(
                ReleaseDeliver {
                    release_id: "v1111111111111111".into(),
                    deployment_id: "d1111111111111111".into(),
                    note: None,
                },
                "uid:1000",
                "2026-09-03T12:00:00Z",
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ParamsInvalid);
        let state: (String, i64) = database
            .call(|connection| {
                Ok((
                    connection.query_row(
                        "SELECT status FROM releases WHERE release_id='v1111111111111111'",
                        [],
                        |row| row.get(0),
                    )?,
                    connection
                        .query_row("SELECT count(*) FROM plan_events", [], |row| row.get(0))?,
                ))
            })
            .expect("unchanged state");
        assert_eq!(state, ("planned".into(), 0));
    }

    #[test]
    fn decisions_are_searchable_supersedable_and_permanently_summarized() {
        let (_temporary, database, service) = world();
        seed_repository(&database);
        let first = service
            .record_decision(
                "r1111111111111111",
                DecisionRecord {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    aspect: DecisionAspect::Architecture,
                    title: "Use the first delivery route".into(),
                    body: "The first route is selected for the fixture deployment.".into(),
                    technical_note: None,
                    r#ref: Some("FIXTURE-FIRST-ROUTE".into()),
                    supersedes: None,
                },
                "uid:1000",
                "2026-09-03T12:00:00Z",
            )
            .expect("first decision");
        let second = service
            .record_decision(
                "r1111111111111111",
                DecisionRecord {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    aspect: DecisionAspect::Architecture,
                    title: "Use the declared delivery route".into(),
                    body: "The declared route replaces the first-port behavior safely.".into(),
                    technical_note: Some("Fixture-only decision".into()),
                    r#ref: Some("FIXTURE-DECLARED-ROUTE".into()),
                    supersedes: first.r#ref.clone(),
                },
                "uid:1000",
                "2026-09-03T12:01:00Z",
            )
            .expect("second decision");
        assert_eq!(second.seq, 2);

        let search = service
            .search_decisions(
                "r1111111111111111",
                DecisionSearchParams {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    query: "declared route".into(),
                    aspect: None,
                    n: 10,
                },
            )
            .expect("search");
        assert_eq!(search.decisions.len(), 1);
        assert_eq!(search.decisions[0].decision_id, second.decision_id);

        let tail = service
            .decision_tail(
                "r1111111111111111",
                DecisionTailParams {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    aspect: None,
                    n: 10,
                    before_seq: None,
                },
            )
            .expect("tail");
        assert_eq!(tail.decisions.len(), 2);
        assert_eq!(
            tail.decisions[0].superseded_by,
            Some(second.decision_id.clone())
        );

        let summary = service
            .summarize_decisions(
                "r1111111111111111",
                DecisionSummarize {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    body: "The declared delivery route is now the durable direction.".into(),
                    covers_through_seq: 2,
                },
                "uid:1000",
                "2026-09-03T12:02:00Z",
            )
            .expect("summarize");
        assert_eq!(summary.unsummarized_count, 0);
        assert!(
            service
                .summarize_decisions(
                    "r1111111111111111",
                    DecisionSummarize {
                        path: None,
                        repository_id: Some("r1111111111111111".into()),
                        body: "This duplicate summary must be rejected permanently.".into(),
                        covers_through_seq: 2,
                    },
                    "uid:1000",
                    "2026-09-03T12:03:00Z",
                )
                .is_err()
        );
    }

    #[test]
    fn task_search_finds_dropped_history_with_bounded_stable_pages() {
        let (_temporary, database, service) = world();
        seed_repository(&database);
        let mut ids = Vec::new();
        for title in ["Archived export", "Current export", "Other work"] {
            ids.push(
                service
                    .create_task(
                        "r1111111111111111",
                        task_params(title),
                        "uid:1000",
                        "2026-09-12T12:00:00Z",
                    )
                    .unwrap()
                    .task_id,
            );
        }
        service
            .update_task(
                serde_json::from_value(serde_json::json!({"task_id": ids[0], "status": "dropped"}))
                    .unwrap(),
                "uid:1000",
                "2026-09-12T12:01:00Z",
            )
            .unwrap();
        let search = |query: &str, after, status| {
            service
                .search_tasks(
                    "r1111111111111111",
                    devcoordinator2_api::params::TaskSearch {
                        path: None,
                        repository_id: None,
                        query: query.into(),
                        status,
                        after_sequence: after,
                        limit: 1,
                    },
                )
                .unwrap()
        };
        let first = search("export", 0, None);
        assert_eq!(first.tasks[0].task_id, ids[0]);
        assert_eq!(first.tasks[0].status, TaskStatus::Dropped);
        let second = search("export", first.next_sequence.unwrap(), None);
        assert_eq!(second.tasks[0].task_id, ids[1]);
        assert!(second.next_sequence.is_none());
        assert_eq!(search("", 0, Some(TaskStatus::Dropped)).tasks.len(), 1);
        assert!(search("%", 0, None).tasks.is_empty());
        let PlanOverview::Detail(overview) = service.overview(Some("r1111111111111111")).unwrap()
        else {
            panic!("detail");
        };
        assert!(overview.tasks.iter().all(|task| task.task_id != ids[0]));
        assert_eq!(
            service.task_history(&ids[0]).unwrap().task.status,
            TaskStatus::Dropped
        );
    }

    #[test]
    fn tasks_keep_order_history_and_atomic_elaboration_rules() {
        let (_temporary, database, service) = world();
        seed_repository(&database);
        let parent = service
            .create_task(
                "r1111111111111111",
                task_params("Deliver owner exports"),
                "uid:1000",
                "2026-09-03T12:00:00Z",
            )
            .expect("parent");
        let mut first_params = task_params("Export the first report");
        first_params.parent_task_id = Some(parent.task_id.clone());
        let first = service
            .create_task(
                "r1111111111111111",
                first_params,
                "uid:1000",
                "2026-09-03T12:01:00Z",
            )
            .expect("first child");
        let mut second_params = task_params("Export the second report");
        second_params.parent_task_id = Some(parent.task_id.clone());
        let second = service
            .create_task(
                "r1111111111111111",
                second_params,
                "uid:1000",
                "2026-09-03T12:02:00Z",
            )
            .expect("second child");
        assert_eq!((first.position, second.position), (1, 2));

        let requested = service
            .update_task(
                TaskUpdate {
                    task_id: first.task_id.clone(),
                    title: None,
                    outcome: None,
                    impact: None,
                    unblock_condition: None,
                    verification: None,
                    technical_note: None,
                    estimated_loc: None,
                    status: Some(TaskStatus::InProgress),
                    release_id: None,
                    parent_task_id: None,
                    position: Some(1),
                    elaboration_needed: Some(true),
                    note: Some("Owner asked for clearer wording.".into()),
                },
                "uid:1000",
                "2026-09-03T12:03:00Z",
            )
            .expect("request elaboration");
        assert!(requested.elaboration_needed);
        assert_eq!(requested.position, 2);
        assert!(
            service
                .update_task(
                    TaskUpdate {
                        task_id: first.task_id.clone(),
                        title: None,
                        outcome: None,
                        impact: None,
                        unblock_condition: None,
                        verification: None,
                        technical_note: None,
                        estimated_loc: None,
                        status: None,
                        release_id: None,
                        parent_task_id: None,
                        position: None,
                        elaboration_needed: Some(false),
                        note: None,
                    },
                    "uid:1000",
                    "2026-09-03T12:04:00Z",
                )
                .is_err()
        );
        let completed = service
            .update_task(
                TaskUpdate {
                    task_id: first.task_id.clone(),
                    title: Some("Let owners export the first report".into()),
                    outcome: Some(
                        "Owners can export the first report in the selected format.".into(),
                    ),
                    impact: None,
                    unblock_condition: None,
                    verification: None,
                    technical_note: None,
                    estimated_loc: Some(120),
                    status: Some(TaskStatus::Done),
                    release_id: None,
                    parent_task_id: None,
                    position: None,
                    elaboration_needed: Some(false),
                    note: Some("Verified through the owner journey.".into()),
                },
                "uid:1000",
                "2026-09-03T12:05:00Z",
            )
            .expect("complete elaboration");
        assert!(!completed.elaboration_needed);
        let history = service.task_history(&first.task_id).expect("history");
        let kinds = history
            .events
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "created",
                "status",
                "elaboration_requested",
                "reorder",
                "estimate",
                "status",
                "edited",
                "elaboration_completed"
            ]
        );
        assert!(history.elaboration_requests.is_empty());
    }

    #[test]
    fn release_lifecycle_and_overview_use_leaf_work() {
        let (_temporary, database, service) = world();
        seed_repository(&database);
        let release = service
            .create_release(
                "r1111111111111111",
                ReleaseCreate {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    name: "First release".into(),
                    kind: ReleaseKind::Release,
                    note: None,
                    seq: None,
                },
                "uid:1000",
                "2026-09-03T13:00:00Z",
            )
            .expect("release");
        let mut parent_params = task_params("Group the export work");
        parent_params.release_id = Some(release.release_id.clone());
        parent_params.estimated_loc = Some(500);
        let parent = service
            .create_task(
                "r1111111111111111",
                parent_params,
                "uid:1000",
                "2026-09-03T13:01:00Z",
            )
            .expect("parent");
        let mut leaf_params = task_params("Deliver the export file");
        leaf_params.parent_task_id = Some(parent.task_id);
        leaf_params.release_id = Some(release.release_id.clone());
        leaf_params.estimated_loc = Some(120);
        let leaf = service
            .create_task(
                "r1111111111111111",
                leaf_params,
                "uid:1000",
                "2026-09-03T13:02:00Z",
            )
            .expect("leaf");
        service
            .update_task(
                TaskUpdate {
                    task_id: leaf.task_id,
                    title: None,
                    outcome: None,
                    impact: None,
                    unblock_condition: None,
                    verification: None,
                    technical_note: None,
                    estimated_loc: None,
                    status: Some(TaskStatus::Done),
                    release_id: None,
                    parent_task_id: None,
                    position: None,
                    elaboration_needed: None,
                    note: None,
                },
                "uid:1000",
                "2026-09-03T13:03:00Z",
            )
            .expect("complete leaf");
        let PlanOverview::Detail(detail) =
            service.overview(Some("r1111111111111111")).expect("detail")
        else {
            panic!("expected plan detail")
        };
        assert_eq!(detail.releases[0].loc_total, 120);
        assert_eq!(detail.releases[0].loc_done, 120);
        assert_eq!(detail.releases[0].tasks_total, 1);

        let updated = service
            .update_release(
                ReleaseUpdate {
                    release_id: release.release_id.clone(),
                    name: Some("Renamed release".into()),
                    seq: Some(2),
                    note: Some("Owner-visible release note".into()),
                    status: None,
                },
                "uid:1000",
                "2026-09-03T13:04:00Z",
            )
            .expect("update release");
        assert_eq!((updated.name.as_str(), updated.seq), ("Renamed release", 2));
        let preview = service
            .request_release(
                "r1111111111111111",
                ReleaseRequest {
                    path: None,
                    repository_id: Some("r1111111111111111".into()),
                    name: None,
                    note: None,
                },
                "uid:1000",
                "2026-09-03T13:05:00Z",
            )
            .expect("preview request");
        assert_eq!(preview.status, ReleaseStatus::Requested);
        assert!(
            service
                .request_release(
                    "r1111111111111111",
                    ReleaseRequest {
                        path: None,
                        repository_id: Some("r1111111111111111".into()),
                        name: None,
                        note: None,
                    },
                    "uid:1000",
                    "2026-09-03T13:06:00Z",
                )
                .is_err()
        );
        let PlanOverview::Collection(collection) = service.overview(None).expect("collection")
        else {
            panic!("expected collection")
        };
        assert_eq!(collection.repositories[0].loc_total, 120);
        assert!(collection.repositories[0].preview_requested);
    }
}
