//! Schema-15 managed and observed deployment state.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devcoordinator2_api::results::{
    CompletedService, Component, ComponentBinding, DeploymentListRow, DeploymentSource,
    DeploymentStatus, DomainChanged,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{format_description::FormatItem, macros::format_description};

use crate::database::{Database, DatabaseError};
use crate::platform::{Clock, HostClock};
use crate::repository_config::{ComponentKind, ComponentSpec, DeploymentSpec};

const OBSERVATION_SOURCE: &str = "legacy-current-import";
const TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

#[derive(Clone, Debug, PartialEq)]
pub struct DeploymentRow {
    pub deployment_id: String,
    pub repository_id: String,
    pub worktree_id: String,
    pub name: String,
    pub source: String,
    pub domain: Option<String>,
    pub spec_fingerprint: String,
    pub spec_json: String,
    pub state: String,
    pub current_generation: Option<u32>,
    pub previous_generation: Option<u32>,
    pub created_at: String,
    pub created_by_uid: u32,
    pub client: String,
    pub updated_at: String,
    pub ttl_expires_at: Option<String>,
    pub public: bool,
    pub domain_override: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComponentRow {
    pub deployment_id: String,
    pub name: String,
    pub kind: String,
    pub order_index: u32,
    pub spec_fingerprint: String,
    pub desired_state: String,
    pub state: String,
    pub health: String,
    pub generation: Option<u32>,
    pub binding_kind: Option<String>,
    pub binding_identity: Option<String>,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationRow {
    pub deployment_id: String,
    pub number: u32,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub path: PathBuf,
    pub fingerprint: String,
    pub created_at: String,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredDeploymentTarget {
    pub repository_id: String,
    pub worktree_id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComponentRuntimePatch {
    pub spec_fingerprint: Option<String>,
    pub desired_state: Option<String>,
    pub state: Option<String>,
    pub health: Option<String>,
    pub generation: Option<Option<u32>>,
    pub binding_kind: Option<Option<String>>,
    pub binding_identity: Option<Option<String>>,
    pub restarts: Option<u32>,
    pub last_error: Option<Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeploymentRuntimePatch {
    pub state: Option<String>,
    pub current_generation: Option<Option<u32>>,
    pub previous_generation: Option<Option<u32>>,
    pub spec_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeCompletionInput {
    pub service: String,
    pub container_id: String,
    pub image_id: Option<String>,
    pub exit_code: i32,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedDeploymentInput {
    pub deployment_id: String,
    pub repository_id: String,
    pub name: String,
    pub native_project: String,
    pub state: String,
    pub health: String,
    #[serde(default)]
    pub evidence: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedContainerInput {
    pub container_id: String,
    pub deployment_id: String,
    pub repository_id: String,
    pub name: String,
    pub image: String,
    pub compose_service: String,
    pub status: String,
    pub health: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedRouteInput {
    pub domain: String,
    pub deployment_id: String,
    pub component: String,
    pub port: u16,
    pub public: bool,
    #[serde(default)]
    pub evidence: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservedImportResult {
    pub deployments: u32,
    pub containers: u32,
    pub routes: u32,
}

#[derive(Clone)]
pub struct DeploymentStore {
    database: Database,
    clock: Arc<dyn Clock>,
}

impl DeploymentStore {
    pub fn new(database: Database) -> Self {
        Self::with_clock(database, Arc::new(HostClock))
    }

    pub fn with_clock(database: Database, clock: Arc<dyn Clock>) -> Self {
        Self { database, clock }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn current_timestamp(&self) -> Result<String, ProtocolError> {
        self.timestamp()
    }

    pub fn deployment_id(worktree_id: &str, name: &str, source: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"devcoordinator2.deployment\0");
        digest.update(worktree_id.as_bytes());
        digest.update([0]);
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(source.as_bytes());
        format!("d{}", &lower_hex(&digest.finalize())[..16])
    }

    pub fn fingerprint(value: &Value) -> String {
        let canonical = serde_json::to_vec(value).expect("JSON values are serializable");
        lower_hex(&Sha256::digest(canonical))[..24].to_owned()
    }

    pub fn component_fingerprint(specification: &ComponentSpec) -> String {
        Self::fingerprint(
            &serde_json::to_value(specification).expect("component specification is serializable"),
        )
    }

    pub fn is_generation_scoped(specification: &ComponentSpec) -> bool {
        specification.kind == ComponentKind::Process
            || (specification.kind == ComponentKind::Docker && specification.volumes.is_empty())
    }

    pub fn is_owned(specification: &ComponentSpec) -> bool {
        specification.kind != ComponentKind::External
            && !(specification.kind == ComponentKind::Postgres
                && specification.shared_from.is_some())
    }

    pub fn get(&self, deployment_id: &str) -> Result<Option<DeploymentRow>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public,domain_override FROM deployments WHERE deployment_id=?1",
                        [&deployment_id],
                        deployment_row,
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    pub fn find(
        &self,
        worktree_id: &str,
        name: &str,
        source: &str,
    ) -> Result<Option<DeploymentRow>, ProtocolError> {
        let worktree_id = worktree_id.to_owned();
        let name = name.to_owned();
        let source = source.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public,domain_override FROM deployments WHERE worktree_id=?1 AND name=?2 AND source=?3",
                        rusqlite::params![worktree_id, name, source],
                        deployment_row,
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    pub fn list(&self, repository_id: Option<&str>) -> Result<Vec<DeploymentRow>, ProtocolError> {
        let repository_id = repository_id.map(str::to_owned);
        self.database
            .call(move |connection| {
                let (sql, values): (&str, Vec<rusqlite::types::Value>) = match repository_id {
                    Some(repository_id) => (
                        "SELECT deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public,domain_override FROM deployments WHERE repository_id=?1 ORDER BY name,source",
                        vec![repository_id.into()],
                    ),
                    None => (
                        "SELECT deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public,domain_override FROM deployments ORDER BY repository_id,name,source",
                        Vec::new(),
                    ),
                };
                let mut statement = connection.prepare(sql)?;
                Ok(statement
                    .query_map(rusqlite::params_from_iter(values), deployment_row)?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    pub fn components(&self, deployment_id: &str) -> Result<Vec<ComponentRow>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,binding_kind,binding_identity,restarts,last_error,updated_at FROM components WHERE deployment_id=?1 ORDER BY order_index",
                )?;
                Ok(statement
                    .query_map([deployment_id], component_row)?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        deployment_id: &str,
        target: &RegisteredDeploymentTarget,
        specification: &DeploymentSpec,
        source: &str,
        domain: Option<&str>,
        state: &str,
        caller_uid: u32,
        client: &str,
        ttl_expires_at: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let fingerprint = specification.fingerprint(source);
        self.upsert_with_fingerprint(
            deployment_id,
            target,
            specification,
            source,
            &fingerprint,
            domain,
            state,
            caller_uid,
            client,
            ttl_expires_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_with_fingerprint(
        &self,
        deployment_id: &str,
        target: &RegisteredDeploymentTarget,
        specification: &DeploymentSpec,
        source: &str,
        spec_fingerprint: &str,
        domain: Option<&str>,
        state: &str,
        caller_uid: u32,
        client: &str,
        ttl_expires_at: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let target = target.clone();
        let name = specification.name.clone();
        let source = source.to_owned();
        let domain = domain.map(str::to_owned);
        let spec_json =
            serde_json::to_string(&specification.canonical(&source)).map_err(|error| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "cannot encode deployment specification",
                )
                .with_detail(error.to_string())
            })?;
        let spec_fingerprint = spec_fingerprint.to_owned();
        let state = state.to_owned();
        let client = client.to_owned();
        let ttl_expires_at = ttl_expires_at.map(str::to_owned);
        let public = specification.public;
        let components = specification.components.clone();
        self.database
            .transaction(move |transaction| {
                let exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM deployments WHERE deployment_id=?1)",
                    [&deployment_id],
                    |row| row.get::<_, i64>(0),
                )? != 0;
                if exists {
                    transaction.execute(
                        "UPDATE deployments SET domain=?1,spec_fingerprint=?2,spec_json=?3,state=?4,updated_at=?5,ttl_expires_at=?6,public=?7 WHERE deployment_id=?8",
                        rusqlite::params![domain,spec_fingerprint,spec_json,state,now,ttl_expires_at,i64::from(public),deployment_id],
                    )?;
                } else {
                    transaction.execute(
                        "INSERT INTO deployments(deployment_id,repository_id,worktree_id,name,source,domain,spec_fingerprint,spec_json,state,current_generation,previous_generation,created_at,created_by_uid,client,updated_at,ttl_expires_at,public) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,NULL,?10,?11,?12,?10,?13,?14)",
                        rusqlite::params![deployment_id,target.repository_id,target.worktree_id,name,source,domain,spec_fingerprint,spec_json,state,now,caller_uid,client,ttl_expires_at,i64::from(public)],
                    )?;
                }
                let names = components
                    .iter()
                    .map(|component| component.name.clone())
                    .collect::<HashSet<_>>();
                for component in &components {
                    transaction.execute(
                        "INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,binding_kind,binding_identity,restarts,last_error,updated_at) VALUES(?1,?2,?3,?4,?5,'running','unknown','unknown',NULL,NULL,NULL,0,NULL,?6) ON CONFLICT(deployment_id,name) DO UPDATE SET type=excluded.type,order_index=excluded.order_index,spec_fingerprint=excluded.spec_fingerprint,updated_at=excluded.updated_at",
                        rusqlite::params![deployment_id,component.name,component.kind.as_str(),u32::try_from(component.order).unwrap_or(u32::MAX),Self::component_fingerprint(component),now],
                    )?;
                    if component.kind == ComponentKind::Compose {
                        let independent = component
                            .independent_services
                            .iter()
                            .cloned()
                            .collect::<HashSet<_>>();
                        for service in &component.independent_services {
                            transaction.execute(
                                "INSERT INTO compose_service_desires(deployment_id,component,service,desired_state,updated_at) VALUES(?1,?2,?3,'running',?4) ON CONFLICT(deployment_id,component,service) DO UPDATE SET desired_state='running',updated_at=excluded.updated_at",
                                rusqlite::params![deployment_id,component.name,service,now],
                            )?;
                        }
                        let mut statement = transaction.prepare(
                            "SELECT service FROM compose_service_desires WHERE deployment_id=?1 AND component=?2",
                        )?;
                        let existing = statement
                            .query_map(rusqlite::params![deployment_id,component.name], |row| row.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()?;
                        drop(statement);
                        for service in existing {
                            if !independent.contains(&service) {
                                transaction.execute(
                                    "DELETE FROM compose_service_desires WHERE deployment_id=?1 AND component=?2 AND service=?3",
                                    rusqlite::params![deployment_id,component.name,service],
                                )?;
                            }
                        }
                    }
                }
                let mut statement = transaction.prepare(
                    "SELECT name FROM components WHERE deployment_id=?1",
                )?;
                let existing = statement
                    .query_map([&deployment_id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                for component in existing {
                    if !names.contains(&component) {
                        transaction.execute(
                            "DELETE FROM compose_service_desires WHERE deployment_id=?1 AND component=?2",
                            rusqlite::params![deployment_id,component],
                        )?;
                        transaction.execute(
                            "DELETE FROM components WHERE deployment_id=?1 AND name=?2",
                            rusqlite::params![deployment_id,component],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn set_deployment_runtime(
        &self,
        deployment_id: &str,
        state: &str,
        current_generation: Option<u32>,
        previous_generation: Option<u32>,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let state = state.to_owned();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE deployments SET state=?1,current_generation=?2,previous_generation=?3,updated_at=?4 WHERE deployment_id=?5",
                    rusqlite::params![state,current_generation,previous_generation,now,deployment_id],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn patch_deployment_runtime(
        &self,
        deployment_id: &str,
        patch: DeploymentRuntimePatch,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        self.database
            .transaction(move |transaction| {
                let changed = transaction.execute(
                    "UPDATE deployments SET state=COALESCE(?1,state),current_generation=CASE WHEN ?2 THEN ?3 ELSE current_generation END,previous_generation=CASE WHEN ?4 THEN ?5 ELSE previous_generation END,spec_fingerprint=COALESCE(?6,spec_fingerprint),updated_at=?7 WHERE deployment_id=?8",
                    rusqlite::params![
                        patch.state,
                        patch.current_generation.is_some(),
                        patch.current_generation.flatten(),
                        patch.previous_generation.is_some(),
                        patch.previous_generation.flatten(),
                        patch.spec_fingerprint,
                        now,
                        deployment_id,
                    ],
                )?;
                if changed == 0 {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        "deployment does not exist",
                    ));
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn restore_apply_snapshot(
        &self,
        deployment: &DeploymentRow,
        components: &[ComponentRow],
        compose_desires: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<(), ProtocolError> {
        let deployment = deployment.clone();
        let components = components.to_vec();
        let compose_desires = compose_desires.clone();
        self.database
            .transaction(move |transaction| {
                let changed = transaction.execute(
                    "UPDATE deployments SET domain=?1,spec_fingerprint=?2,spec_json=?3,state=?4,current_generation=?5,previous_generation=?6,updated_at=?7,ttl_expires_at=?8,public=?9,domain_override=?10 WHERE deployment_id=?11",
                    rusqlite::params![
                        deployment.domain,
                        deployment.spec_fingerprint,
                        deployment.spec_json,
                        deployment.state,
                        deployment.current_generation,
                        deployment.previous_generation,
                        deployment.updated_at,
                        deployment.ttl_expires_at,
                        i64::from(deployment.public),
                        deployment.domain_override,
                        deployment.deployment_id,
                    ],
                )?;
                if changed == 0 {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        "deployment does not exist",
                    ));
                }
                transaction.execute(
                    "DELETE FROM compose_service_desires WHERE deployment_id=?1",
                    [&deployment.deployment_id],
                )?;
                transaction.execute(
                    "DELETE FROM components WHERE deployment_id=?1",
                    [&deployment.deployment_id],
                )?;
                for component in components {
                    transaction.execute(
                        "INSERT INTO components(deployment_id,name,type,order_index,spec_fingerprint,desired_state,state,health,generation,binding_kind,binding_identity,restarts,last_error,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                        rusqlite::params![
                            component.deployment_id,
                            component.name,
                            component.kind,
                            component.order_index,
                            component.spec_fingerprint,
                            component.desired_state,
                            component.state,
                            component.health,
                            component.generation,
                            component.binding_kind,
                            component.binding_identity,
                            component.restarts,
                            component.last_error,
                            component.updated_at,
                        ],
                    )?;
                }
                for (component, desires) in compose_desires {
                    for (service, state) in desires {
                        transaction.execute(
                            "INSERT INTO compose_service_desires(deployment_id,component,service,desired_state,updated_at) VALUES(?1,?2,?3,?4,?5)",
                            rusqlite::params![
                                deployment.deployment_id,
                                component,
                                service,
                                state,
                                deployment.updated_at,
                            ],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn set_public(&self, deployment_id: &str, public: bool) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        self.database
            .transaction(move |transaction| {
                let changed = transaction.execute(
                    "UPDATE deployments SET public=?1,updated_at=?2 WHERE deployment_id=?3",
                    rusqlite::params![i64::from(public), now, deployment_id],
                )?;
                if changed == 0 {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        "deployment does not exist",
                    ));
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn set_component_runtime(
        &self,
        deployment_id: &str,
        name: &str,
        patch: ComponentRuntimePatch,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let name = name.to_owned();
        self.database
            .transaction(move |transaction| {
                let changed = transaction.execute(
                    "UPDATE components SET spec_fingerprint=COALESCE(?1,spec_fingerprint),desired_state=COALESCE(?2,desired_state),state=COALESCE(?3,state),health=COALESCE(?4,health),generation=CASE WHEN ?5 THEN ?6 ELSE generation END,binding_kind=CASE WHEN ?7 THEN ?8 ELSE binding_kind END,binding_identity=CASE WHEN ?9 THEN ?10 ELSE binding_identity END,restarts=COALESCE(?11,restarts),last_error=CASE WHEN ?12 THEN ?13 ELSE last_error END,updated_at=?14 WHERE deployment_id=?15 AND name=?16",
                    rusqlite::params![
                        patch.spec_fingerprint,
                        patch.desired_state,
                        patch.state,
                        patch.health,
                        patch.generation.is_some(),
                        patch.generation.flatten(),
                        patch.binding_kind.is_some(),
                        patch.binding_kind.flatten(),
                        patch.binding_identity.is_some(),
                        patch.binding_identity.flatten(),
                        patch.restarts,
                        patch.last_error.is_some(),
                        patch.last_error.flatten(),
                        now,
                        deployment_id,
                        name,
                    ],
                )?;
                if changed == 0 {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        "deployment component does not exist",
                    ));
                }
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn add_generation(
        &self,
        deployment_id: &str,
        number: u32,
        commit_hash: Option<&str>,
        dirty: bool,
        path: &Path,
        fingerprint: &str,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let commit_hash = commit_hash.map(str::to_owned);
        let path = path.to_string_lossy().into_owned();
        let fingerprint = fingerprint.to_owned();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT OR REPLACE INTO generations(deployment_id,number,commit_hash,dirty,path,fingerprint,created_at,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'candidate')",
                    rusqlite::params![deployment_id,number,commit_hash,i64::from(dirty),path,fingerprint,now],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn generation(
        &self,
        deployment_id: &str,
        number: u32,
    ) -> Result<Option<GenerationRow>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT deployment_id,number,commit_hash,dirty,path,fingerprint,created_at,state FROM generations WHERE deployment_id=?1 AND number=?2",
                        rusqlite::params![deployment_id,number],
                        generation_row,
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    pub fn generations(&self, deployment_id: &str) -> Result<Vec<GenerationRow>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT deployment_id,number,commit_hash,dirty,path,fingerprint,created_at,state FROM generations WHERE deployment_id=?1 ORDER BY number",
                )?;
                Ok(statement
                    .query_map([deployment_id], generation_row)?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    pub fn set_generation_state(
        &self,
        deployment_id: &str,
        number: u32,
        state: &str,
    ) -> Result<(), ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        let state = state.to_owned();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE generations SET state=?1 WHERE deployment_id=?2 AND number=?3",
                    rusqlite::params![state, deployment_id, number],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn prune_generations(
        &self,
        deployment_id: &str,
        keep: &BTreeSet<u32>,
    ) -> Result<Vec<GenerationRow>, ProtocolError> {
        let deployment_id_owned = deployment_id.to_owned();
        let rows = self.database.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT deployment_id,number,commit_hash,dirty,path,fingerprint,created_at,state FROM generations WHERE deployment_id=?1 ORDER BY number",
            )?;
            Ok(statement
                .query_map([deployment_id_owned], generation_row)?
                .collect::<Result<Vec<_>, _>>()?)
        }).map_err(database_error)?;
        let stale = rows
            .into_iter()
            .filter(|row| !keep.contains(&row.number))
            .collect::<Vec<_>>();
        let deployment_id = deployment_id.to_owned();
        let numbers = stale.iter().map(|row| row.number).collect::<Vec<_>>();
        self.database
            .transaction(move |transaction| {
                for number in numbers {
                    transaction.execute(
                        "DELETE FROM compose_completions WHERE deployment_id=?1 AND generation=?2",
                        rusqlite::params![deployment_id, number],
                    )?;
                    transaction.execute(
                        "DELETE FROM generations WHERE deployment_id=?1 AND number=?2",
                        rusqlite::params![deployment_id, number],
                    )?;
                }
                Ok(())
            })
            .map_err(database_error)?;
        Ok(stale)
    }

    pub fn set_route(
        &self,
        domain: Option<&str>,
        deployment_id: &str,
        component: Option<&str>,
        port: Option<u16>,
        generation: Option<u32>,
    ) -> Result<(), ProtocolError> {
        let now = self.timestamp()?;
        let domain = domain.map(str::to_owned);
        let deployment_id = deployment_id.to_owned();
        let component = component.map(str::to_owned);
        let conflict = self.database.transaction(move |transaction| {
            if let Some(domain) = domain {
                let component = component.ok_or_else(|| domain_error(ErrorCode::ParamsInvalid, "route component is required"))?;
                let other: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM domain_routes WHERE domain=?1 AND deployment_id!=?2 UNION ALL SELECT 1 FROM observed_routes WHERE domain=?1)",
                    rusqlite::params![domain,deployment_id], |row| row.get(0),
                )?;
                let lease_id = match port {
                    Some(port) => crate::ports::route_lease(transaction, &deployment_id, &component, port, generation, None)?,
                    None => None,
                };
                if other || (port.is_some() && lease_id.is_none()) {
                    crate::ports::withdraw_conflict(transaction, &deployment_id, &component)?;
                    return Ok(true);
                }
                transaction.execute("DELETE FROM domain_routes WHERE deployment_id=?1", [&deployment_id])?;
                transaction.execute(
                    "INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![domain,deployment_id,component,port,generation,now,lease_id],
                )?;
                if port.is_some() {
                    transaction.execute("UPDATE deployments SET current_generation=?1 WHERE deployment_id=?2 AND state='applying'", rusqlite::params![generation,deployment_id])?;
                }
            } else {
                transaction.execute("DELETE FROM domain_routes WHERE deployment_id=?1", [&deployment_id])?;
            }
            Ok(false)
        }).map_err(database_error)?;
        if conflict {
            return Err(ProtocolError::new(
                ErrorCode::RouteLeaseConflict,
                "route was withdrawn because its lease, owner, port or healthy generation does not match",
            ));
        }
        Ok(())
    }

    pub fn effective_domain(
        row: Option<&DeploymentRow>,
        specification: &DeploymentSpec,
        source: &str,
    ) -> Option<String> {
        row.and_then(|row| row.domain_override.clone())
            .or_else(|| specification.domain_for(source).map(str::to_owned))
    }

    pub fn domain_owner(&self, domain: &str) -> Result<Option<String>, ProtocolError> {
        let domain = domain.to_owned();
        self.database
            .call(move |connection| {
                let managed = connection
                    .query_row(
                        "SELECT deployment_id FROM domain_routes WHERE domain=?1",
                        [&domain],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if managed.is_some() {
                    return Ok(managed);
                }
                connection
                    .query_row(
                        "SELECT observed_deployment_id FROM observed_routes WHERE domain=?1",
                        [&domain],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)
    }

    pub fn override_managed_domain(
        &self,
        deployment_id: &str,
        domain_override: Option<&str>,
    ) -> Result<DomainChanged, ProtocolError> {
        if let Some(domain) = domain_override
            && !valid_domain(domain)
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "domain must be a lowercase DNS label",
            ));
        }
        let row = self.get(deployment_id)?.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::DeploymentNotFound,
                format!("no deployment {deployment_id}"),
            )
        })?;
        let specification: Value = serde_json::from_str(&row.spec_json).map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "stored deployment specification is invalid",
            )
            .with_detail(error.to_string())
        })?;
        let declared_domain = specification
            .get("domain")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let effective = domain_override
            .map(str::to_owned)
            .or_else(|| declared_domain.clone());
        let deployment_id_for_route = deployment_id.to_owned();
        let route = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT component,port,generation FROM domain_routes WHERE deployment_id=?1",
                        [&deployment_id_for_route],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<u16>>(1)?,
                                row.get::<_, Option<u32>>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        let target = if route.is_none() && effective.is_some() {
            Some(self.route_target_from_stored_spec(&row, &specification)?)
        } else {
            None
        };
        let route_port = route
            .as_ref()
            .and_then(|(_, port, _)| *port)
            .or_else(|| target.as_ref().and_then(|(_, port, _)| *port));
        let had_route = route.is_some();
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let stored_override = domain_override.map(str::to_owned);
        let stored_effective = effective.clone();
        self.database
            .transaction(move |transaction| {
                transaction.execute(
                    "UPDATE deployments SET domain_override=?1,domain=?2,updated_at=?3 WHERE deployment_id=?4",
                    rusqlite::params![stored_override,stored_effective,now,deployment_id],
                )?;
                if had_route {
                    if let Some(domain) = stored_effective {
                        transaction.execute(
                            "UPDATE domain_routes SET domain=?1,published_at=?2 WHERE deployment_id=?3",
                            rusqlite::params![domain,now,deployment_id],
                        )?;
                    } else {
                        transaction.execute(
                            "DELETE FROM domain_routes WHERE deployment_id=?1",
                            [&deployment_id],
                        )?;
                    }
                } else if let (Some(domain), Some((component, port, generation))) =
                    (stored_effective, target)
                {
                    let lease = match port {
                        Some(port) => crate::ports::route_lease(transaction, &deployment_id, &component, port, generation, None)?,
                        None => None,
                    };
                    if port.is_some() && lease.is_none() {
                        return Err(domain_error(ErrorCode::RouteLeaseConflict, "the route target has no matching healthy lease"));
                    }
                    transaction.execute(
                        "INSERT INTO domain_routes(domain,deployment_id,component,port,generation,published_at,lease_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        rusqlite::params![domain,deployment_id,component,port,generation,now,lease],
                    )?;
                }
                Ok(())
            })
            .map_err(database_error)?;
        Ok(DomainChanged {
            deployment_id: row.deployment_id,
            domain: effective,
            domain_source: Some(if domain_override.is_some() {
                "override".into()
            } else {
                "configuration".into()
            }),
            declared_domain,
            route_port,
            public: Some(row.public),
            observed_only: Some(false),
        })
    }

    fn route_target_from_stored_spec(
        &self,
        row: &DeploymentRow,
        specification: &Value,
    ) -> Result<(String, Option<u16>, Option<u32>), ProtocolError> {
        let components = specification
            .get("components")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "stored deployment components are invalid",
                )
            })?;
        let mut route_component = components.iter().find_map(|component| {
            component
                .get("route")
                .and_then(Value::as_bool)
                .filter(|route| *route)
                .and_then(|_| component.get("name").and_then(Value::as_str))
                .map(str::to_owned)
        });
        if route_component.is_none() {
            let implicit = components
                .iter()
                .filter(|component| {
                    component.get("wants_port").and_then(Value::as_bool) == Some(true)
                        && component
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| matches!(kind, "process" | "docker"))
                })
                .filter_map(|component| component.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if implicit.len() == 1 {
                route_component = implicit.into_iter().next();
            }
        }
        let component = route_component.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "this deployment has no routable component; mark one port-leasing process or Docker component with route = true and apply",
            )
        })?;
        let generation = row.current_generation.unwrap_or(0);
        let deployment_id = row.deployment_id.clone();
        let query_component = component.clone();
        let port = self
            .database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT port FROM port_assignments WHERE deployment_id=?1 AND component=?2 AND generation IN (?3,0) ORDER BY generation DESC LIMIT 1",
                        rusqlite::params![deployment_id,query_component,generation],
                        |row| row.get::<_,u16>(0),
                    )
                    .optional()
                    .map_err(DatabaseError::from)
            })
            .map_err(database_error)?;
        Ok((component, port, (generation != 0).then_some(generation)))
    }

    pub fn delete_rows(&self, deployment_id: &str) -> Result<(), ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .transaction(move |transaction| {
                for table in [
                    "domain_routes",
                    "port_assignments",
                    "compose_completions",
                    "compose_service_desires",
                    "components",
                    "generations",
                ] {
                    transaction.execute(
                        &format!("DELETE FROM {table} WHERE deployment_id=?1"),
                        [&deployment_id],
                    )?;
                }
                transaction.execute(
                    "DELETE FROM deployments WHERE deployment_id=?1",
                    [&deployment_id],
                )?;
                Ok(())
            })
            .map_err(database_error)
    }

    pub fn record_compose_completions(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
        candidates: &[ComposeCompletionInput],
    ) -> Result<(), ProtocolError> {
        if candidates.is_empty() {
            return Ok(());
        }
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let component = component.to_owned();
        let candidates = candidates.to_vec();
        self.database.transaction(move |transaction| {
            for candidate in candidates {
                transaction.execute(
                    "INSERT OR REPLACE INTO compose_completions(deployment_id,component,service,generation,container_id,image_id,exit_code,started_at,finished_at,recorded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![deployment_id,component,candidate.service,generation,candidate.container_id,candidate.image_id,candidate.exit_code,candidate.started_at,candidate.finished_at,now],
                )?;
            }
            Ok(())
        }).map_err(database_error)
    }

    pub fn compose_completions(
        &self,
        deployment_id: &str,
        component: &str,
        generation: u32,
    ) -> Result<BTreeMap<String, CompletedService>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        let component = component.to_owned();
        self.database.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT service,generation,container_id,image_id,exit_code,started_at,finished_at,recorded_at FROM compose_completions WHERE deployment_id=?1 AND component=?2 AND generation=?3 ORDER BY service",
            )?;
            let rows = statement.query_map(rusqlite::params![deployment_id,component,generation], |row| {
                Ok(CompletedService {
                    service: row.get(0)?,
                    generation: row.get(1)?,
                    container_id: row.get(2)?,
                    image_id: row.get(3)?,
                    exit_code: row.get(4)?,
                    started_at: row.get(5)?,
                    finished_at: row.get(6)?,
                    recorded_at: row.get(7)?,
                })
            })?;
            let mut result = BTreeMap::new();
            for row in rows {
                let row = row?;
                result.insert(row.service.clone(), row);
            }
            Ok(result)
        }).map_err(database_error)
    }

    pub fn set_compose_service_desired(
        &self,
        deployment_id: &str,
        component: &str,
        service: &str,
        desired_state: &str,
    ) -> Result<(), ProtocolError> {
        if !matches!(desired_state, "running" | "stopped") {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                format!("invalid Compose service desired state {desired_state:?}"),
            ));
        }
        let now = self.timestamp()?;
        let deployment_id = deployment_id.to_owned();
        let component = component.to_owned();
        let service = service.to_owned();
        let desired_state = desired_state.to_owned();
        self.database.transaction(move |transaction| {
            transaction.execute(
                "INSERT INTO compose_service_desires(deployment_id,component,service,desired_state,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(deployment_id,component,service) DO UPDATE SET desired_state=excluded.desired_state,updated_at=excluded.updated_at",
                rusqlite::params![deployment_id,component,service,desired_state,now],
            )?;
            Ok(())
        }).map_err(database_error)
    }

    pub fn compose_service_desires(
        &self,
        deployment_id: &str,
        component: &str,
    ) -> Result<BTreeMap<String, String>, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        let component = component.to_owned();
        self.database.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT service,desired_state FROM compose_service_desires WHERE deployment_id=?1 AND component=?2 ORDER BY service",
            )?;
            Ok(statement
                .query_map(rusqlite::params![deployment_id,component], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<BTreeMap<_,_>,_>>()?)
        }).map_err(database_error)
    }

    pub fn managed_list_rows(
        &self,
        active_repositories: &HashSet<String>,
    ) -> Result<Vec<DeploymentListRow>, ProtocolError> {
        let active_repositories = active_repositories.clone();
        self.database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT d.deployment_id,d.repository_id,r.display_name,d.name,d.source,d.state,d.domain,d.public,d.current_generation,dr.port,d.updated_at,d.ttl_expires_at FROM deployments d JOIN repositories r ON r.repository_id=d.repository_id LEFT JOIN domain_routes dr ON dr.deployment_id=d.deployment_id WHERE r.archived_at IS NULL ORDER BY d.repository_id,d.name,d.source",
                )?;
                let mut rows = Vec::new();
                for row in statement.query_map([], |row| {
                    let source: String = row.get(4)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        source,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)? != 0,
                        row.get::<_, Option<u32>>(8)?,
                        row.get::<_, Option<u16>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, Option<String>>(11)?,
                    ))
                })? {
                    let (
                        deployment_id,
                        repository_id,
                        repository_name,
                        name,
                        source,
                        state,
                        domain,
                        public,
                        current_generation,
                        route_port,
                        updated_at,
                        ttl_expires_at,
                    ) = row?;
                    if !active_repositories.contains(&repository_id) {
                        continue;
                    }
                    rows.push(DeploymentListRow {
                        deployment_id,
                        repository_id,
                        repository_name: Some(repository_name),
                        name,
                        source: deployment_source(&source)?,
                        state,
                        domain,
                        public,
                        current_generation,
                        route_port,
                        updated_at,
                        ttl_expires_at,
                        observed_only: false,
                        health: None,
                    });
                }
                Ok(rows)
            })
            .map_err(database_error)
    }

    pub fn observed_exists(&self, deployment_id: &str) -> Result<bool, ProtocolError> {
        let deployment_id = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM observed_deployments WHERE observed_deployment_id=?1)",
                    [&deployment_id],
                    |row| row.get::<_, i64>(0),
                )? != 0)
            })
            .map_err(database_error)
    }

    pub fn observed_list(
        &self,
        repository_id: Option<&str>,
    ) -> Result<Vec<DeploymentListRow>, ProtocolError> {
        let repository_id = repository_id.map(str::to_owned);
        self.database
            .call(move |connection| {
                let mut sql = "SELECT d.observed_deployment_id,d.repository_id,rep.display_name,d.name,d.state,d.health,r.domain,r.port,r.public,d.observed_at FROM observed_deployments d LEFT JOIN observed_routes r ON r.observed_deployment_id=d.observed_deployment_id LEFT JOIN repositories rep ON rep.repository_id=d.repository_id".to_owned();
                let mut values = Vec::<rusqlite::types::Value>::new();
                if let Some(repository_id) = repository_id {
                    sql.push_str(" WHERE d.repository_id=?1");
                    values.push(repository_id.into());
                }
                sql.push_str(" ORDER BY d.repository_id,d.name,d.observed_deployment_id");
                let mut statement = connection.prepare(&sql)?;
                Ok(statement
                    .query_map(rusqlite::params_from_iter(values), |row| {
                        Ok(DeploymentListRow {
                            deployment_id: row.get(0)?,
                            repository_id: row.get(1)?,
                            repository_name: row.get(2)?,
                            name: row.get(3)?,
                            source: DeploymentSource::Observed,
                            state: row.get(4)?,
                            health: row.get(5)?,
                            domain: row.get(6)?,
                            route_port: row.get(7)?,
                            public: row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
                            current_generation: None,
                            updated_at: row.get(9)?,
                            ttl_expires_at: None,
                            observed_only: true,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(database_error)
    }

    pub fn observed_status(
        &self,
        deployment_id: &str,
    ) -> Result<Option<DeploymentStatus>, ProtocolError> {
        let deployment_id_owned = deployment_id.to_owned();
        self.database
            .call(move |connection| {
                let row = connection
                    .query_row(
                        "SELECT d.repository_id,rep.display_name,d.name,d.native_project,d.state,d.health,d.source,d.observed_at,r.domain,r.port,r.component,r.public FROM observed_deployments d LEFT JOIN observed_routes r ON r.observed_deployment_id=d.observed_deployment_id LEFT JOIN repositories rep ON rep.repository_id=d.repository_id WHERE d.observed_deployment_id=?1",
                        [&deployment_id_owned],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, String>(6)?,
                                row.get::<_, String>(7)?,
                                row.get::<_, Option<String>>(8)?,
                                row.get::<_, Option<u16>>(9)?,
                                row.get::<_, Option<String>>(10)?,
                                row.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
                            ))
                        },
                    )
                    .optional()?;
                let Some((repository_id,repository_name,name,native_project,state,health,source,observed_at,domain,route_port,route_component,public)) = row else {
                    return Ok(None);
                };
                let mut statement = connection.prepare(
                    "SELECT container_id,name,compose_service,state,status,health FROM observed_containers WHERE observed_deployment_id=?1 ORDER BY compose_service,name,container_id",
                )?;
                let components = statement
                    .query_map([&deployment_id_owned], |row| {
                        let container_id: String = row.get(0)?;
                        let display_name: String = row.get(1)?;
                        let component_name: String = row.get(2)?;
                        let component_health: String = row.get(5)?;
                        let status: String = row.get(4)?;
                        Ok(Component {
                            lease_id: None,
                            name: component_name.clone(),
                            display_name: Some(display_name),
                            r#type: "container".into(),
                            state: row.get(3)?,
                            health: component_health.clone(),
                            generation: None,
                            binding: ComponentBinding {
                                kind: Some("observed-container".into()),
                                identity: Some(container_id),
                            },
                            port: (route_component.as_deref() == Some(&component_name))
                                .then_some(route_port)
                                .flatten(),
                            restarts: None,
                            owned: false,
                            independent_control: false,
                            last_error: (component_health == "unhealthy").then_some(status),
                            services: None,
                            completed_services: None,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(DeploymentStatus {
                    deployment_id: deployment_id_owned,
                    repository_id,
                    repository_name,
                    name,
                    source: DeploymentSource::Observed,
                    state,
                    health: Some(health),
                    current_generation: None,
                    previous_generation: None,
                    domain,
                    route_port,
                    route_component,
                    public,
                    ttl_expires_at: None,
                    components,
                    log_dir: None,
                    observed_only: true,
                    native_project: Some(native_project),
                    observation_source: Some(source),
                    observed_at: Some(observed_at),
                    unchanged: None,
                    rolled_back_from: None,
                    rolled_back_to: None,
                    readiness: None,
                }))
            })
            .map_err(database_error)
    }

    pub fn replace_observed_current(
        &self,
        deployments: &[ObservedDeploymentInput],
        containers: &[ObservedContainerInput],
        routes: &[ObservedRouteInput],
        observed_at: &str,
    ) -> Result<ObservedImportResult, ProtocolError> {
        let deployments = deployments.to_vec();
        let containers = containers.to_vec();
        let routes = routes.to_vec();
        let observed_at = observed_at.to_owned();
        let imported_at = observed_at.clone();
        self.database
            .transaction(move |transaction| {
                transaction.execute("DELETE FROM observed_routes", [])?;
                transaction.execute("DELETE FROM observed_containers", [])?;
                transaction.execute("DELETE FROM observed_deployments", [])?;
                for deployment in &deployments {
                    transaction.execute(
                        "INSERT INTO observed_deployments(observed_deployment_id,repository_id,name,native_project,state,health,source,evidence_json,observed_at,imported_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                        rusqlite::params![deployment.deployment_id,deployment.repository_id,deployment.name,deployment.native_project,deployment.state,deployment.health,OBSERVATION_SOURCE,canonical_json(&deployment.evidence)?,observed_at,imported_at],
                    )?;
                }
                for container in &containers {
                    transaction.execute(
                        "INSERT INTO observed_containers(container_id,observed_deployment_id,repository_id,name,image,compose_service,state,status,health,observed_at) VALUES(?1,?2,?3,?4,?5,?6,'running',?7,?8,?9)",
                        rusqlite::params![container.container_id,container.deployment_id,container.repository_id,container.name,container.image,container.compose_service,container.status,container.health,observed_at],
                    )?;
                }
                for route in &routes {
                    let conflict = transaction.query_row(
                        "SELECT deployment_id FROM domain_routes WHERE domain=?1",
                        [&route.domain],
                        |row| row.get::<_, String>(0),
                    ).optional()?;
                    if conflict.is_some() {
                        return Err(domain_error(
                            ErrorCode::ParamsInvalid,
                            format!("observed route {} conflicts with a managed route", route.domain),
                        ));
                    }
                    transaction.execute(
                        "INSERT INTO observed_routes(domain,observed_deployment_id,component,port,public,evidence_json,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        rusqlite::params![route.domain,route.deployment_id,route.component,route.port,i64::from(route.public),canonical_json(&route.evidence)?,observed_at],
                    )?;
                }
                Ok(ObservedImportResult {
                    deployments: u32::try_from(deployments.len()).unwrap_or(u32::MAX),
                    containers: u32::try_from(containers.len()).unwrap_or(u32::MAX),
                    routes: u32::try_from(routes.len()).unwrap_or(u32::MAX),
                })
            })
            .map_err(database_error)
    }

    pub fn set_observed_domain(
        &self,
        deployment_id: &str,
        domain: Option<&str>,
        port: Option<u16>,
        component: Option<&str>,
        public: Option<bool>,
    ) -> Result<DomainChanged, ProtocolError> {
        if let Some(domain) = domain
            && !valid_domain(domain)
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "domain must be a lowercase DNS label",
            ));
        }
        let now = self.timestamp()?;
        let deployment_id_owned = deployment_id.to_owned();
        let domain = domain.map(str::to_owned);
        let component = component.map(str::to_owned);
        self.database
            .transaction(move |transaction| {
                let exists = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM observed_deployments WHERE observed_deployment_id=?1)",
                    [&deployment_id_owned],
                    |row| row.get::<_, i64>(0),
                )? != 0;
                if !exists {
                    return Err(domain_error(
                        ErrorCode::DeploymentNotFound,
                        format!("no deployment {deployment_id_owned}"),
                    ));
                }
                let route = transaction.query_row(
                    "SELECT port,component,public FROM observed_routes WHERE observed_deployment_id=?1",
                    [&deployment_id_owned],
                    |row| Ok((row.get::<_,u16>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)? != 0)),
                ).optional()?;
                if domain.is_none() {
                    transaction.execute(
                        "DELETE FROM observed_routes WHERE observed_deployment_id=?1",
                        [&deployment_id_owned],
                    )?;
                    return Ok(DomainChanged {
                        deployment_id: deployment_id_owned,
                        domain: None,
                        domain_source: None,
                        declared_domain: None,
                        route_port: None,
                        public: Some(false),
                        observed_only: Some(true),
                    });
                }
                let domain = domain.expect("checked");
                let had_route = route.is_some();
                let (final_port,final_component,final_public) = match route {
                    Some((existing_port,existing_component,existing_public)) => (
                        port.unwrap_or(existing_port),
                        existing_component,
                        public.unwrap_or(existing_public),
                    ),
                    None => {
                        let port = port.ok_or_else(|| domain_error(
                            ErrorCode::ParamsInvalid,
                            "this observed deployment publishes no route yet; pass 'port' together with 'domain'",
                        ))?;
                        let mut statement = transaction.prepare(
                            "SELECT compose_service FROM observed_containers WHERE observed_deployment_id=?1 ORDER BY compose_service,name,container_id",
                        )?;
                        let containers = statement
                            .query_map([&deployment_id_owned], |row| row.get::<_,String>(0))?
                            .collect::<Result<Vec<_>,_>>()?;
                        drop(statement);
                        if containers.is_empty() {
                            return Err(domain_error(
                                ErrorCode::DeploymentActionFailed,
                                "no containers are recorded for this observed deployment",
                            ));
                        }
                        let component = match component {
                            Some(component) => component,
                            None if containers.len() == 1 => containers[0].clone(),
                            None => return Err(domain_error(
                                ErrorCode::ParamsInvalid,
                                "'component' is required (several containers exist)",
                            )),
                        };
                        (port, component, public.unwrap_or(false))
                    }
                };
                if had_route {
                    transaction.execute(
                        "UPDATE observed_routes SET domain=?1,component=?2,port=?3,public=?4,evidence_json=?5,observed_at=?6 WHERE observed_deployment_id=?7",
                        rusqlite::params![domain,final_component,final_port,i64::from(final_public),"{\"set_by\":\"deployment.set_domain\"}",now,deployment_id_owned],
                    )?;
                } else {
                    transaction.execute(
                        "INSERT INTO observed_routes(domain,observed_deployment_id,component,port,public,evidence_json,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        rusqlite::params![domain,deployment_id_owned,final_component,final_port,i64::from(final_public),"{\"set_by\":\"deployment.set_domain\"}",now],
                    )?;
                }
                Ok(DomainChanged {
                    deployment_id: deployment_id_owned,
                    domain: Some(domain),
                    domain_source: None,
                    declared_domain: None,
                    route_port: Some(final_port),
                    public: Some(final_public),
                    observed_only: Some(true),
                })
            })
            .map_err(database_error)
    }

    fn timestamp(&self) -> Result<String, ProtocolError> {
        self.clock
            .now_utc()
            .format(TIMESTAMP_FORMAT)
            .map_err(|error| {
                ProtocolError::new(
                    ErrorCode::InternalError,
                    "cannot format deployment timestamp",
                )
                .with_detail(error.to_string())
            })
    }
}

fn deployment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeploymentRow> {
    Ok(DeploymentRow {
        deployment_id: row.get(0)?,
        repository_id: row.get(1)?,
        worktree_id: row.get(2)?,
        name: row.get(3)?,
        source: row.get(4)?,
        domain: row.get(5)?,
        spec_fingerprint: row.get(6)?,
        spec_json: row.get(7)?,
        state: row.get(8)?,
        current_generation: row.get(9)?,
        previous_generation: row.get(10)?,
        created_at: row.get(11)?,
        created_by_uid: row.get(12)?,
        client: row.get(13)?,
        updated_at: row.get(14)?,
        ttl_expires_at: row.get(15)?,
        public: row.get::<_, i64>(16)? != 0,
        domain_override: row.get(17)?,
    })
}

fn component_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ComponentRow> {
    Ok(ComponentRow {
        deployment_id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        order_index: row.get(3)?,
        spec_fingerprint: row.get(4)?,
        desired_state: row.get(5)?,
        state: row.get(6)?,
        health: row.get(7)?,
        generation: row.get(8)?,
        binding_kind: row.get(9)?,
        binding_identity: row.get(10)?,
        restarts: row.get(11)?,
        last_error: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn generation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GenerationRow> {
    Ok(GenerationRow {
        deployment_id: row.get(0)?,
        number: row.get(1)?,
        commit_hash: row.get(2)?,
        dirty: row.get::<_, i64>(3)? != 0,
        path: PathBuf::from(row.get::<_, String>(4)?),
        fingerprint: row.get(5)?,
        created_at: row.get(6)?,
        state: row.get(7)?,
    })
}

fn deployment_source(value: &str) -> Result<DeploymentSource, DatabaseError> {
    match value {
        "worktree" => Ok(DeploymentSource::Worktree),
        "checkout" => Ok(DeploymentSource::Checkout),
        _ => Err(domain_error(
            ErrorCode::InternalError,
            "stored deployment source is invalid",
        )),
    }
}

fn canonical_json(value: &Value) -> Result<String, DatabaseError> {
    serde_json::to_string(value).map_err(|error| {
        DatabaseError::Domain(
            ProtocolError::new(ErrorCode::InternalError, "cannot encode observed evidence")
                .with_detail(error.to_string()),
        )
    })
}

fn valid_domain(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("String writes cannot fail");
    }
    result
}

fn domain_error(code: ErrorCode, message: impl Into<String>) -> DatabaseError {
    DatabaseError::Domain(ProtocolError::new(code, message))
}

fn database_error(error: DatabaseError) -> ProtocolError {
    match error {
        DatabaseError::Domain(error) => error,
        other => ProtocolError::new(
            ErrorCode::InternalError,
            "deployment database operation failed",
        )
        .with_detail(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_config::load_deployment_spec;
    use serde_json::json;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn world() -> (tempfile::TempDir, Database, DeploymentStore, DeploymentSpec) {
        let temporary = tempdir().expect("tempdir");
        std::fs::write(
            temporary.path().join(".devcoordinator.toml"),
            r#"
schema=2
[deployment.web]
domain="app"
components=["api","stack"]
[deployment.web.component.api]
type="process"
command=["serve"]
port=true
route=true
[deployment.web.component.stack]
type="compose"
services=["bootstrap","worker"]
finite_services=["bootstrap"]
independent_services=["worker"]
"#,
        )
        .expect("config");
        let specification = load_deployment_spec(temporary.path(), "web").expect("deployment spec");
        let database = Database::open(temporary.path().join("authority.sqlite3")).expect("db");
        database
            .transaction(move |transaction| {
                transaction.execute("INSERT INTO repositories(repository_id,root_path,display_name,registered_at,registered_by_uid,last_seen_at) VALUES('r1111111111111111','/repo','repo','t',1,'t')", [])?;
                transaction.execute("INSERT INTO worktrees(worktree_id,repository_id,worktree_path,registered_at,last_seen_at) VALUES('w1111111111111111','r1111111111111111','/repo','t','t')", [])?;
                Ok(())
            })
            .expect("repository");
        let store = DeploymentStore::with_clock(
            database.clone(),
            Arc::new(crate::platform::FixedClock(datetime!(2026-09-04 01:00 UTC))),
        );
        (temporary, database, store, specification)
    }

    fn target() -> RegisteredDeploymentTarget {
        RegisteredDeploymentTarget {
            repository_id: "r1111111111111111".into(),
            worktree_id: "w1111111111111111".into(),
        }
    }

    #[test]
    fn managed_rows_generations_routes_completions_and_desires_are_lossless() {
        let (temporary, database, store, specification) = world();
        let deployment_id = DeploymentStore::deployment_id("w1111111111111111", "web", "worktree");
        assert_eq!(deployment_id.len(), 17);
        store
            .upsert(
                &deployment_id,
                &target(),
                &specification,
                "worktree",
                Some("app"),
                "stopped",
                1000,
                "codex",
                None,
            )
            .expect("upsert");
        let row = store.get(&deployment_id).unwrap().unwrap();
        assert_eq!(row.spec_fingerprint.len(), 24);
        assert_eq!(row.domain.as_deref(), Some("app"));
        assert_eq!(store.components(&deployment_id).unwrap().len(), 2);
        assert_eq!(
            store
                .compose_service_desires(&deployment_id, "stack")
                .unwrap(),
            BTreeMap::from([("worker".into(), "running".into())])
        );
        store
            .set_compose_service_desired(&deployment_id, "stack", "worker", "stopped")
            .unwrap();
        store
            .set_component_runtime(
                &deployment_id,
                "api",
                ComponentRuntimePatch {
                    state: Some("running".into()),
                    health: Some("healthy".into()),
                    generation: Some(Some(1)),
                    binding_kind: Some(Some("unit".into())),
                    binding_identity: Some(Some("unit.service".into())),
                    ..ComponentRuntimePatch::default()
                },
            )
            .unwrap();
        store
            .add_generation(
                &deployment_id,
                1,
                Some("abc"),
                true,
                temporary.path(),
                "fingerprint-one",
            )
            .unwrap();
        store
            .add_generation(
                &deployment_id,
                2,
                Some("def"),
                false,
                temporary.path(),
                "fingerprint-two",
            )
            .unwrap();
        store
            .set_deployment_runtime(&deployment_id, "running", Some(2), Some(1))
            .unwrap();
        store
            .set_generation_state(&deployment_id, 2, "current")
            .unwrap();
        store
            .set_component_runtime(
                &deployment_id,
                "api",
                ComponentRuntimePatch {
                    generation: Some(Some(2)),
                    ..Default::default()
                },
            )
            .unwrap();
        let route_deployment_id = deployment_id.clone();
        database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(20001,?1,?2,?3,?4,?5)",
                    rusqlite::params![route_deployment_id, "api", 0, "t", "lfixture"],
                )?;
                Ok(())
            })
            .unwrap();
        store
            .record_compose_completions(
                &deployment_id,
                "stack",
                2,
                &[ComposeCompletionInput {
                    service: "bootstrap".into(),
                    container_id: "a".repeat(64),
                    image_id: Some("sha256:image".into()),
                    exit_code: 0,
                    started_at: Some("start".into()),
                    finished_at: Some("finish".into()),
                }],
            )
            .unwrap();
        assert_eq!(
            store
                .compose_completions(&deployment_id, "stack", 2)
                .unwrap()["bootstrap"]
                .exit_code,
            0
        );
        store
            .set_route(
                Some("app"),
                &deployment_id,
                Some("api"),
                Some(20001),
                Some(2),
            )
            .unwrap();
        assert_eq!(
            store.domain_owner("app").unwrap(),
            Some(deployment_id.clone())
        );
        let changed = store
            .override_managed_domain(&deployment_id, Some("renamed"))
            .unwrap();
        assert_eq!(changed.domain.as_deref(), Some("renamed"));
        assert_eq!(changed.route_port, Some(20001));
        let restored = store.override_managed_domain(&deployment_id, None).unwrap();
        assert_eq!(restored.domain.as_deref(), Some("app"));
        let stale = store
            .prune_generations(&deployment_id, &BTreeSet::from([2]))
            .unwrap();
        assert_eq!(
            stale.iter().map(|row| row.number).collect::<Vec<_>>(),
            vec![1]
        );
        assert!(store.generation(&deployment_id, 1).unwrap().is_none());
        let listed = store
            .managed_list_rows(&HashSet::from(["r1111111111111111".into()]))
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source, DeploymentSource::Worktree);
        assert_eq!(listed[0].route_port, Some(20001));
    }

    #[test]
    fn upsert_reconciles_removed_components_and_independent_services() {
        let (_temporary, _database, store, mut specification) = world();
        let deployment_id = DeploymentStore::deployment_id("w1111111111111111", "web", "worktree");
        store
            .upsert(
                &deployment_id,
                &target(),
                &specification,
                "worktree",
                Some("app"),
                "stopped",
                1,
                "other",
                None,
            )
            .unwrap();
        specification
            .components
            .retain(|component| component.name == "stack");
        specification.components[0].independent_services.clear();
        store
            .upsert(
                &deployment_id,
                &target(),
                &specification,
                "worktree",
                Some("app"),
                "stopped",
                1,
                "other",
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .components(&deployment_id)
                .unwrap()
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["stack"]
        );
        assert!(
            store
                .compose_service_desires(&deployment_id, "stack")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn observed_replacement_status_and_domain_changes_are_atomic_and_typed() {
        let (_temporary, database, store, specification) = world();
        let managed = DeploymentStore::deployment_id("w1111111111111111", "web", "worktree");
        store
            .upsert(
                &managed,
                &target(),
                &specification,
                "worktree",
                Some("managed"),
                "running",
                1,
                "other",
                None,
            )
            .unwrap();
        store
            .set_deployment_runtime(&managed, "running", Some(1), None)
            .unwrap();
        let route_deployment_id = managed.clone();
        database
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO port_assignments(port,deployment_id,component,generation,assigned_at,lease_id) VALUES(20000,?1,?2,?3,?4,?5)",
                    rusqlite::params![route_deployment_id, "api", 0, "t", "lmanaged"],
                )?;
                Ok(())
            })
            .unwrap();
        store
            .set_component_runtime(
                &managed,
                "api",
                ComponentRuntimePatch {
                    state: Some("running".into()),
                    health: Some("healthy".into()),
                    generation: Some(Some(1)),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .set_route(Some("managed"), &managed, Some("api"), Some(20000), Some(1))
            .unwrap();
        let deployment = ObservedDeploymentInput {
            deployment_id: "d2222222222222222".into(),
            repository_id: "r1111111111111111".into(),
            name: "native".into(),
            native_project: "native-project".into(),
            state: "running".into(),
            health: "healthy".into(),
            evidence: json!({"source":"fixture"}),
        };
        let container = ObservedContainerInput {
            container_id: "b".repeat(64),
            deployment_id: deployment.deployment_id.clone(),
            repository_id: deployment.repository_id.clone(),
            name: "native-api".into(),
            image: "image:tag".into(),
            compose_service: "api".into(),
            status: "running".into(),
            health: "healthy".into(),
        };
        let imported = store
            .replace_observed_current(
                std::slice::from_ref(&deployment),
                std::slice::from_ref(&container),
                &[],
                "2026-09-04T01:00:00Z",
            )
            .unwrap();
        assert_eq!(imported.deployments, 1);
        let changed = store
            .set_observed_domain(
                &deployment.deployment_id,
                Some("native"),
                Some(20002),
                None,
                Some(true),
            )
            .unwrap();
        assert_eq!(
            (changed.route_port, changed.public),
            (Some(20002), Some(true))
        );
        let status = store
            .observed_status(&deployment.deployment_id)
            .unwrap()
            .unwrap();
        assert!(status.observed_only);
        assert_eq!(
            status.components[0].binding.identity.as_deref(),
            Some(container.container_id.as_str())
        );
        assert_eq!(status.components[0].port, Some(20002));
        assert_eq!(store.observed_list(None).unwrap().len(), 1);

        let conflict = store
            .replace_observed_current(
                std::slice::from_ref(&deployment),
                std::slice::from_ref(&container),
                &[ObservedRouteInput {
                    domain: "managed".into(),
                    deployment_id: deployment.deployment_id.clone(),
                    component: "api".into(),
                    port: 20003,
                    public: false,
                    evidence: json!({}),
                }],
                "2026-09-04T02:00:00Z",
            )
            .expect_err("managed conflict");
        assert_eq!(conflict.code, ErrorCode::ParamsInvalid);
        let count: u32 = database
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM observed_deployments",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "failed replacement rolled back its initial deletes"
        );
        store
            .set_observed_domain(&deployment.deployment_id, None, None, None, None)
            .unwrap();
        assert_eq!(
            store
                .observed_status(&deployment.deployment_id)
                .unwrap()
                .unwrap()
                .domain,
            None
        );
    }

    #[test]
    fn domain_ownership_and_invalid_runtime_updates_fail_closed() {
        let (_temporary, _database, store, _specification) = world();
        assert!(
            store
                .set_component_runtime("missing", "api", ComponentRuntimePatch::default())
                .is_err()
        );
        assert!(
            store
                .set_compose_service_desired("missing", "stack", "worker", "paused")
                .is_err()
        );
        assert!(
            store
                .set_observed_domain("missing", Some("Bad"), Some(1), None, None)
                .is_err()
        );
        assert_eq!(
            DeploymentStore::fingerprint(&json!({"b":2,"a":1})).len(),
            24
        );
    }
}
