use devcoordinator2_api::storage as api;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Private native identity, never projected into an inventory or event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Locator {
    Directory {
        path: PathBuf,
        device: u64,
        inode: u64,
        root: PathBuf,
        git_root: Option<PathBuf>,
    },
    Docker {
        object_type: String,
        identity: String,
        created: String,
    },
    Mount {
        source: PathBuf,
        target: PathBuf,
        entry: String,
        device: u64,
        inode: u64,
    },
    Evidence {
        worktree: PathBuf,
        run_id: String,
        leaf: String,
    },
}

/// Private caller-scoped uncertainty, not a transferable successful Git check.
/// Unknown versions/fields fail closed when reading the stored record.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source_owner_observation_version", deny_unknown_fields)]
pub enum SourceOwnerObservation {
    #[serde(rename = "1")]
    Unavailable {
        resource_key: String,
        fingerprint: String,
        git_root: PathBuf,
        origin: SourceOwnerOrigin,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceOwnerOrigin {
    Registered,
    LocalManual { uid: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// Persistent observation/mutation order, independent of wall-clock changes.
    #[serde(default)]
    pub update_sequence: u64,
    pub artifact: api::Artifact,
    pub locator: Locator,
    pub resource_key: String,
    #[serde(default)]
    pub ancestor_keys: Vec<String>,
    pub fingerprint: String,
    pub owner_deployment: Option<String>,
    pub last_activity_signature: String,
    pub discovered_effect: api::Effect,
    pub disposal_approved: bool,
    pub disposal_reason: Option<String>,
    pub blockers: Vec<String>,
    #[serde(default)]
    pub source_owner_observation: Option<SourceOwnerObservation>,
    pub recovery_lineage: Option<String>,
    pub recovery_verified: bool,
    #[serde(default)]
    pub recovery_created_at_ms: Option<u64>,
    pub private_aliases: Vec<PathBuf>,
    #[serde(default)]
    pub retention_eligible: bool,
}

impl Record {
    pub fn source_owner_reason(&self) -> Option<&'static str> {
        self.source_owner_observation.as_ref().map(|observation| {
            let SourceOwnerObservation::Unavailable {
                resource_key, fingerprint, git_root, origin,
            } = observation;
            if resource_key == &self.resource_key && fingerprint == &self.fingerprint
                && !matches!(origin, SourceOwnerOrigin::LocalManual { uid: 0 })
                && matches!(&self.locator, Locator::Directory { git_root: Some(root), .. } if root == git_root)
            {
                "git_execution_owner_unverified"
            } else {
                "stored_storage_record_invalid"
            }
        })
    }

    /// Only an identity-bound annotation can be omitted for a fresh operation
    /// validation. Legacy/shared reason strings are never removed here.
    pub fn for_caller_validation(&self) -> Self {
        let mut current = self.clone();
        if self.source_owner_reason() == Some("git_execution_owner_unverified") {
            current.source_owner_observation = None;
        }
        current
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootRecord {
    pub root: api::Root,
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredPlan {
    pub public: api::CleanupPlan,
    pub records: Vec<Record>,
    pub policies: BTreeMap<String, u64>,
}

/// Private job metadata. Public request parameters and their idempotency digest
/// are unchanged; origin is captured once from the authenticated transport.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "storage_request_version", deny_unknown_fields)]
pub(crate) enum StoredJobRequest {
    #[serde(rename = "1")]
    V1 {
        parameters: serde_json::Value,
        local_manual: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub repository_id: Option<String>,
    pub records: Vec<Record>,
    pub filesystems: Vec<api::Filesystem>,
    pub coverage_gaps: Vec<String>,
    pub complete_kinds: Vec<api::Kind>,
}

#[derive(Clone, Debug, Default)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub execution_uid: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Kernel-authenticated local caller for this manual operation only.
    /// Root, automatic work and Console keep the registered execution owner.
    pub manual_caller_uid: Option<u32>,
    pub discovering: bool,
    pub docker_snapshot: Option<std::sync::Arc<Vec<serde_json::Value>>>,
    pub docker_unavailable: bool,
    pub scan_repository_id: Option<String>,
    pub active_worktrees: Vec<PathBuf>,
    pub repositories: Vec<Repository>,
    pub worktrees: Vec<(String, PathBuf)>,
    pub roots: Vec<RootRecord>,
    pub deployment_repositories: BTreeMap<String, String>,
    pub current_paths: Vec<PathBuf>,
    pub current_container_ids: Vec<String>,
    pub current_projects: BTreeMap<String, String>,
    pub current_images: Vec<String>,
    pub deployment_names: BTreeMap<String, String>,
    pub observed: BTreeMap<String, (String, String)>,
    pub recorded_containers: BTreeMap<String, (String, String)>,
    pub leased_artifacts: Vec<String>,
    pub protected_resources: Vec<String>,
    pub protected_ancestors: Vec<String>,
    pub leased_resources: Vec<String>,
    pub leased_ancestors: Vec<String>,
    pub now_ms: u64,
    pub retention_age_seconds: u64,
    pub retention_depth: u64,
}

pub fn persistent(effect: api::Effect) -> bool {
    matches!(
        effect,
        api::Effect::PermanentData | api::Effect::RecoveryCopy | api::Effect::SourceWorktree
    )
}

pub fn atom<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
