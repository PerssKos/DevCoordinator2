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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
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
    pub recovery_lineage: Option<String>,
    pub recovery_verified: bool,
    #[serde(default)]
    pub recovery_created_at_ms: Option<u64>,
    pub private_aliases: Vec<PathBuf>,
    #[serde(default)]
    pub retention_eligible: bool,
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
