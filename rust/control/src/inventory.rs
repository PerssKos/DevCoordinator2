//! Exact Docker container inventory and safe ephemeral cleanup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use devcoordinator2_api::results::{
    ContainerClassification, ContainerList, ContainerRow, RemovedContainer,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use serde::Deserialize;

use crate::database::{Database, DatabaseError};
use crate::docker::{DockerControl, DockerError, DockerInvocation, ExactContainerId, LABEL_PREFIX};

const CLASSES: &[(&str, ContainerClassification)] = &[
    ("managed-test", ContainerClassification::ManagedTest),
    ("managed-preview", ContainerClassification::ManagedPreview),
    (
        "managed-permanent",
        ContainerClassification::ManagedPermanent,
    ),
    ("observed-current", ContainerClassification::ObservedCurrent),
    ("orphaned-managed", ContainerClassification::OrphanedManaged),
    ("unmanaged", ContainerClassification::Unmanaged),
];

const INVENTORY_LIMIT: usize = 2048;
const INVENTORY_BATCH_SIZE: usize = 32;

#[derive(Clone)]
pub struct ContainerInventory {
    database: Database,
    instance: String,
    docker: Arc<dyn DockerControl>,
}

#[derive(Clone)]
struct KnownComponent {
    deployment_id: String,
    component: String,
    binding_identity: Option<String>,
}

#[derive(Clone)]
struct KnownDeployment {
    deployment_id: String,
    repository_id: String,
}

#[derive(Clone)]
struct ObservedContainer {
    repository_id: String,
    deployment_id: String,
    component: String,
}

#[derive(Deserialize)]
struct DockerRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names", default)]
    name: String,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "CreatedAt", default)]
    created: String,
    #[serde(rename = "Labels", default)]
    labels: String,
}

impl ContainerInventory {
    pub fn new(
        database: Database,
        instance: impl Into<String>,
        docker: Arc<dyn DockerControl>,
    ) -> Self {
        Self {
            database,
            instance: instance.into(),
            docker,
        }
    }

    pub fn containers(&self) -> Result<ContainerList, ProtocolError> {
        let (components, compose_projects, deployments, observed) = self.known_state()?;
        let components = components
            .into_iter()
            .map(|component| {
                (
                    (component.deployment_id, component.component),
                    component.binding_identity,
                )
            })
            .collect::<HashMap<_, _>>();
        let compose_projects = compose_projects.into_iter().collect::<HashSet<_>>();
        let deployments = deployments
            .into_iter()
            .map(|deployment| (deployment.deployment_id.clone(), deployment))
            .collect::<HashMap<_, _>>();
        let observed = observed.into_iter().collect::<HashMap<_, _>>();
        let mut rows = Vec::new();
        for container in self.all_containers()? {
            let identity = ExactContainerId::parse(container.id.clone()).map_err(|_| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "Docker inventory returned a non-exact container identity",
                )
            })?;
            let labels = parse_labels(&container.labels);
            let mut row = ContainerRow {
                id: identity.to_string(),
                name: container.name,
                image: container.image,
                state: container.state,
                status: container.status,
                created: container.created,
                repository_id: None,
                deployment_id: None,
                component: None,
                run_id: None,
                caller_uid: None,
                client: None,
                ttl_seconds: None,
                data: None,
                classification: ContainerClassification::Unmanaged,
                cpu_percent: None,
                memory_bytes: None,
                pids: None,
                container_layer_bytes: None,
            };
            let purpose = labels.get(&format!("{LABEL_PREFIX}.purpose"));
            let compose_project = labels.get("com.docker.compose.project");
            if labels.get(&format!("{LABEL_PREFIX}.instance")) == Some(&self.instance)
                && purpose.is_some()
            {
                row.repository_id = labels.get(&format!("{LABEL_PREFIX}.repository")).cloned();
                row.deployment_id = labels.get(&format!("{LABEL_PREFIX}.deployment")).cloned();
                row.component = labels.get(&format!("{LABEL_PREFIX}.component")).cloned();
                row.run_id = labels.get(&format!("{LABEL_PREFIX}.run")).cloned();
                row.caller_uid = labels
                    .get(&format!("{LABEL_PREFIX}.caller_uid"))
                    .and_then(|value| value.parse().ok());
                row.client = labels.get(&format!("{LABEL_PREFIX}.client")).cloned();
                row.ttl_seconds = labels
                    .get(&format!("{LABEL_PREFIX}.ttl_seconds"))
                    .and_then(|value| value.parse().ok());
                row.data = labels.get(&format!("{LABEL_PREFIX}.data")).cloned();
                row.classification = if purpose.is_some_and(|purpose| purpose == "test") {
                    ContainerClassification::ManagedTest
                } else {
                    let current = row
                        .deployment_id
                        .as_ref()
                        .zip(row.component.as_ref())
                        .and_then(|key| components.get(&(key.0.clone(), key.1.clone())))
                        .and_then(Option::as_deref);
                    if current == Some(row.id.as_str()) {
                        if purpose.is_some_and(|purpose| purpose == "preview") {
                            ContainerClassification::ManagedPreview
                        } else {
                            ContainerClassification::ManagedPermanent
                        }
                    } else {
                        ContainerClassification::OrphanedManaged
                    }
                };
            } else if let Some(project) = compose_project
                && compose_projects.contains(project)
            {
                let deployment = deployments.values().find(|deployment| {
                    project.starts_with(&format!("dc2-{}-", deployment.deployment_id))
                });
                row.deployment_id = deployment.map(|deployment| deployment.deployment_id.clone());
                row.repository_id = deployment.map(|deployment| deployment.repository_id.clone());
                row.component = project.rsplit('-').next().map(str::to_owned);
                row.classification = if deployment.is_some() {
                    ContainerClassification::ManagedPermanent
                } else {
                    ContainerClassification::OrphanedManaged
                };
            } else if let Some(imported) = observed.get(&row.id) {
                row.repository_id = Some(imported.repository_id.clone());
                row.deployment_id = Some(imported.deployment_id.clone());
                row.component = Some(imported.component.clone());
                row.client = Some("legacy-current-import".into());
                row.data = Some("observed-only".into());
                row.classification = ContainerClassification::ObservedCurrent;
            }
            rows.push(row);
        }
        Ok(ContainerList {
            counts: counts(&rows),
            containers: rows,
        })
    }

    pub fn remove(&self, container_id: &str) -> Result<RemovedContainer, ProtocolError> {
        let exact = ExactContainerId::parse(container_id.to_owned()).map_err(|_| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "container_id must be a full 64-hex ID",
            )
        })?;
        let row = self
            .containers()?
            .containers
            .into_iter()
            .find(|row| row.id == container_id)
            .ok_or_else(|| ProtocolError::new(ErrorCode::ParamsInvalid, "no such container"))?;
        if !matches!(
            row.classification,
            ContainerClassification::OrphanedManaged | ContainerClassification::ManagedTest
        ) {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                format!(
                    "{} containers are not removed by DevCoordinator; decide manually",
                    classification_name(&row.classification)
                ),
            ));
        }
        self.docker
            .remove_container(&exact, true)
            .map_err(docker_error)?;
        Ok(RemovedContainer {
            container_id: exact.to_string(),
            removed: true,
            classification: row.classification,
        })
    }

    fn all_containers(&self) -> Result<Vec<DockerRow>, ProtocolError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let output = self
            .docker
            .invoke(
                DockerInvocation::new(
                    vec![
                        OsString::from("ps"),
                        OsString::from("--all"),
                        OsString::from("--no-trunc"),
                        OsString::from("--format"),
                        OsString::from("{{json .}}"),
                    ],
                    Duration::from_secs(30),
                )
                .expect("static Docker inventory invocation is valid"),
            )
            .map_err(docker_error)?;
        if output.success() && output.stdout_truncated && !output.stderr_truncated {
            return self.batched_containers(deadline);
        }
        if !output.success() || output.stdout_truncated || output.stderr_truncated {
            return Err(docker_error(DockerError::Command(
                "Docker inventory failed or exceeded its capture bound".into(),
            )));
        }
        let mut rows = Vec::new();
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let row: DockerRow = serde_json::from_str(line).map_err(|_| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "Docker inventory returned malformed JSON",
                )
            })?;
            rows.push(row);
        }
        Ok(rows)
    }

    fn batched_containers(&self, deadline: Instant) -> Result<Vec<DockerRow>, ProtocolError> {
        let query = |arguments| {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    docker_error(DockerError::Command("inventory query timed out".into()))
                })?;
            let output = self
                .docker
                .invoke(DockerInvocation::new(arguments, remaining).map_err(docker_error)?)
                .map_err(docker_error)?;
            if Instant::now() >= deadline
                || !output.success()
                || output.stdout_truncated
                || output.stderr_truncated
            {
                return Err(docker_error(DockerError::Command(
                    "inventory query failed or exceeded its shared deadline or capture bound"
                        .into(),
                )));
            }
            Ok(output.stdout)
        };
        let index = query(vec![
            "ps".into(),
            "--all".into(),
            "--no-trunc".into(),
            "--format".into(),
            "{{.ID}}".into(),
        ])?;
        let mut identities = Vec::new();
        let mut indexed = HashSet::new();
        for identity in index.lines().filter(|line| !line.trim().is_empty()) {
            let identity = ExactContainerId::parse(identity.trim())
                .map_err(docker_error)?
                .to_string();
            if identities.len() == INVENTORY_LIMIT || !indexed.insert(identity.clone()) {
                return Err(docker_error(DockerError::InvalidOutput(
                    "inventory ID index is duplicate or exceeds its bounded container limit".into(),
                )));
            }
            identities.push(identity);
        }
        let mut rows = Vec::new();
        let mut returned = HashSet::new();
        for batch in identities.chunks(INVENTORY_BATCH_SIZE) {
            let mut arguments = vec![
                "ps".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--format".into(),
                "{{json .}}".into(),
            ];
            for identity in batch {
                arguments.push("--filter".into());
                arguments.push(format!("id={identity}").into());
            }
            let output = query(arguments)?;
            for line in output.lines().filter(|line| !line.trim().is_empty()) {
                let row: DockerRow = serde_json::from_str(line).map_err(|_| {
                    docker_error(DockerError::InvalidOutput(
                        "inventory batch contains malformed JSON".into(),
                    ))
                })?;
                let identity = ExactContainerId::parse(row.id.clone())
                    .map_err(docker_error)?
                    .to_string();
                if !batch.contains(&identity) || !returned.insert(identity) {
                    return Err(docker_error(DockerError::InvalidOutput(
                        "inventory batch returned an unrequested or repeated container".into(),
                    )));
                }
                rows.push(row);
            }
        }
        Ok(rows)
    }

    #[allow(clippy::type_complexity)]
    fn known_state(
        &self,
    ) -> Result<
        (
            Vec<KnownComponent>,
            Vec<String>,
            Vec<KnownDeployment>,
            Vec<(String, ObservedContainer)>,
        ),
        ProtocolError,
    > {
        self.database
            .call(|connection| {
                let components = {
                    let mut statement = connection.prepare(
                        "SELECT deployment_id,name,binding_identity FROM components",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok(KnownComponent {
                                deployment_id: row.get(0)?,
                                component: row.get(1)?,
                                binding_identity: row.get(2)?,
                            })
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let compose = {
                    let mut statement = connection.prepare(
                        "SELECT binding_identity FROM components WHERE binding_kind='compose' AND binding_identity IS NOT NULL",
                    )?;
                    statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let deployments = {
                    let mut statement = connection
                        .prepare("SELECT deployment_id,repository_id FROM deployments")?;
                    statement
                        .query_map([], |row| {
                            Ok(KnownDeployment {
                                deployment_id: row.get(0)?,
                                repository_id: row.get(1)?,
                            })
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let observed = {
                    let mut statement = connection.prepare(
                        "SELECT container_id,repository_id,observed_deployment_id,compose_service FROM observed_containers",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((
                                row.get(0)?,
                                ObservedContainer {
                                    repository_id: row.get(1)?,
                                    deployment_id: row.get(2)?,
                                    component: row.get(3)?,
                                },
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                Ok((components, compose, deployments, observed))
            })
            .map_err(database_error)
    }
}

fn parse_labels(value: &str) -> HashMap<String, String> {
    value
        .split(',')
        .filter_map(|item| item.split_once('='))
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

fn counts(rows: &[ContainerRow]) -> BTreeMap<String, u32> {
    let mut counts = CLASSES
        .iter()
        .map(|(name, _)| ((*name).into(), 0))
        .collect::<BTreeMap<_, _>>();
    for row in rows {
        *counts
            .entry(classification_name(&row.classification).into())
            .or_default() += 1;
    }
    counts
}

fn classification_name(classification: &ContainerClassification) -> &'static str {
    match classification {
        ContainerClassification::ManagedTest => "managed-test",
        ContainerClassification::ManagedPreview => "managed-preview",
        ContainerClassification::ManagedPermanent => "managed-permanent",
        ContainerClassification::ObservedCurrent => "observed-current",
        ContainerClassification::OrphanedManaged => "orphaned-managed",
        ContainerClassification::Unmanaged => "unmanaged",
    }
}

fn docker_error(error: DockerError) -> ProtocolError {
    ProtocolError::new(ErrorCode::InternalError, "Docker inventory is unavailable")
        .with_detail(error.to_string())
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(
            ErrorCode::InternalError,
            "container inventory state query failed",
        )
        .with_detail(other.to_string()),
    }
}

#[cfg(test)]
#[path = "inventory_batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{DockerOutput, LogFollower};
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct FakeDocker {
        output: String,
        removed: Mutex<Vec<String>>,
    }

    impl DockerControl for FakeDocker {
        fn invoke(&self, invocation: DockerInvocation) -> Result<DockerOutput, DockerError> {
            assert_eq!(
                invocation
                    .args()
                    .iter()
                    .map(|value| value.to_string_lossy())
                    .collect::<Vec<_>>(),
                ["ps", "--all", "--no-trunc", "--format", "{{json .}}"]
            );
            Ok(DockerOutput {
                exit_code: 0,
                stdout: self.output.clone(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }

        fn spawn_follow_logs(
            &self,
            _container_id: &ExactContainerId,
        ) -> Result<LogFollower, DockerError> {
            Err(DockerError::InvalidRequest("unexpected logs".into()))
        }

        fn remove_container(
            &self,
            container_id: &ExactContainerId,
            delete_volumes: bool,
        ) -> Result<(), DockerError> {
            assert!(delete_volumes);
            self.removed.lock().unwrap().push(container_id.to_string());
            Ok(())
        }
    }

    #[test]
    fn classification_and_cleanup_are_exact_and_fail_closed() {
        let temporary = tempdir().unwrap();
        let database = Database::open(temporary.path().join("authority.sqlite3")).unwrap();
        database.transaction(|transaction| {
            transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1,'t')", [])?;
            transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111','/repo','t','t')", [])?;
            transaction.execute("INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,created_at,created_by_uid,client,updated_at) VALUES('d1111111111111111','r1111111111111111','w1111111111111111','web','worktree',NULL,'f','{}','running','t',1,'other','t')", [])?;
            transaction.execute("INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,binding_kind,binding_identity,restarts,updated_at) VALUES('d1111111111111111','api','docker',0,'f','running','running','healthy','container',?1,0,'t')", ["a".repeat(64)])?;
            Ok(())
        }).unwrap();
        let managed = serde_json::json!({
            "ID":"a".repeat(64),"Names":"api","Image":"api:1","State":"running","Status":"Up","CreatedAt":"today",
            "Labels":"devcoordinator2.instance=fixture,devcoordinator2.purpose=permanent,devcoordinator2.repository=r1111111111111111,devcoordinator2.deployment=d1111111111111111,devcoordinator2.component=api"
        });
        let orphan = serde_json::json!({
            "ID":"b".repeat(64),"Names":"old","Image":"api:0","State":"exited","Status":"Exited","CreatedAt":"today",
            "Labels":"devcoordinator2.instance=fixture,devcoordinator2.purpose=preview,devcoordinator2.repository=r1111111111111111,devcoordinator2.deployment=d1111111111111111,devcoordinator2.component=api"
        });
        let unmanaged = serde_json::json!({
            "ID":"c".repeat(64),"Names":"foreign","Image":"x","State":"running","Status":"Up","CreatedAt":"today","Labels":""
        });
        let docker = Arc::new(FakeDocker {
            output: format!("{managed}\n{orphan}\n{unmanaged}\n"),
            removed: Mutex::new(Vec::new()),
        });
        let inventory = ContainerInventory::new(database, "fixture", docker.clone());
        let rows = inventory.containers().unwrap();
        assert_eq!(
            rows.containers
                .iter()
                .map(|row| classification_name(&row.classification))
                .collect::<Vec<_>>(),
            ["managed-permanent", "orphaned-managed", "unmanaged"]
        );
        assert_eq!(rows.counts["orphaned-managed"], 1);
        assert_eq!(
            inventory.remove(&"a".repeat(64)).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert!(inventory.remove(&"b".repeat(64)).unwrap().removed);
        assert_eq!(docker.removed.lock().unwrap().as_slice(), &["b".repeat(64)]);
        assert_eq!(
            inventory.remove("short").unwrap_err().code,
            ErrorCode::ParamsInvalid
        );
    }
}
