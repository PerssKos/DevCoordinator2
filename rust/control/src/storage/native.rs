//! Native storage observation and exact-target mutations; no shell commands.
mod discovery;
mod docker;
mod docker_engine;
mod evidence;
mod mounts;
mod sources;
use super::{
    blocked, fs, hash, json,
    model::{Context, Discovery, Locator, Record},
    stable_id, unavailable,
};
use crate::{
    config::Config,
    docker::{DockerCli, DockerControl},
    metrics_source::run_bounded,
};
use devcoordinator2_api::{ProtocolError, storage as api};
pub use discovery::{DiscoveryBudget, DiscoveryCursor};
pub(crate) use mounts::execute as run_mount_helper;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub trait Backend: Send + Sync {
    fn discover(&self, context: &Context) -> Result<Discovery, ProtocolError>;
    fn discover_step(
        &self,
        context: &Context,
        cursor: &mut DiscoveryCursor,
        _budget: DiscoveryBudget,
    ) -> Result<Option<Discovery>, ProtocolError> {
        if cursor.complete {
            return Err(unavailable("discovery_already_complete"));
        }
        let result = self.discover(context)?;
        cursor.complete = true;
        Ok(Some(result))
    }
    fn validate(
        &self,
        record: &Record,
        context: &Context,
        selected: &[String],
    ) -> Result<(), ProtocolError>;
    /// Validate a recognized source-worktree disposal intent without running
    /// the unbounded discovery context or full native safety checks.
    fn validate_disposal_intent(
        &self,
        record: &Record,
        repository_id: &str,
        now_ms: u64,
        reviewed_ignored_files: Option<&[api::IgnoredFileReview]>,
    ) -> Result<(String, Option<super::model::IgnoredReview>), ProtocolError>;
    fn remove(
        &self,
        record: &Record,
        context: &Context,
        selected: &[String],
        job_id: &str,
    ) -> Result<(), ProtocolError>;
    /// Used only after a durable mutation intent exists, including restart.
    fn absent(&self, record: &Record, context: &Context) -> Result<bool, ProtocolError>;
    fn absent_step(
        &self,
        record: &Record,
        context: &Context,
        _cursor: &mut Option<String>,
        _budget: DiscoveryBudget,
    ) -> Result<Option<bool>, ProtocolError> {
        self.absent(record, context).map(Some)
    }
    fn save_recovery(&self, record: &Record, job_id: &str) -> Result<(), ProtocolError>;
    fn resume(
        &self,
        record: &Record,
        context: &Context,
        selected: &[String],
        job_id: &str,
    ) -> Result<(), ProtocolError>;
    fn legacy_group(
        &self,
        deployment_id: &str,
        records: &[Record],
        context: &Context,
    ) -> Result<Vec<String>, ProtocolError>;
}

pub struct HostBackend {
    config: Config,
    database: crate::database::Database,
    docker: Arc<dyn DockerControl>,
}

impl HostBackend {
    pub fn new(config: Config, database: crate::database::Database) -> Self {
        Self {
            config,
            database,
            docker: Arc::new(DockerCli::default()),
        }
    }
    pub fn with_docker(
        config: Config,
        database: crate::database::Database,
        docker: Arc<dyn DockerControl>,
    ) -> Self {
        Self {
            config,
            database,
            docker,
        }
    }
}

impl Backend for HostBackend {
    fn resume(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
        job_id: &str,
    ) -> Result<(), ProtocolError> {
        if matches!(r.locator, Locator::Mount { .. }) {
            self.validate_mount_recovery(r, context, selected, job_id)?;
            self.remove_mount(r, job_id)
        } else {
            self.remove(r, context, selected, job_id)
        }
    }
    fn save_recovery(&self, r: &Record, job_id: &str) -> Result<(), ProtocolError> {
        if matches!(r.locator, Locator::Mount { .. }) {
            return self.save_mount_recovery(r, job_id);
        }
        self.save_private_definition(r, job_id)
    }

    fn validate_disposal_intent(
        &self,
        record: &Record,
        repository_id: &str,
        now_ms: u64,
        reviewed_ignored_files: Option<&[api::IgnoredFileReview]>,
    ) -> Result<(String, Option<super::model::IgnoredReview>), ProtocolError> {
        self.validate_intent(record, repository_id, now_ms, reviewed_ignored_files)
    }

    fn absent(&self, r: &Record, context: &Context) -> Result<bool, ProtocolError> {
        match &r.locator {
            Locator::Directory { path, .. } => fs::absent(path),
            Locator::Docker { .. } => self.docker_absent(r, context),
            Locator::Mount { target, .. } => {
                Ok(!self.mount_entries()?.iter().any(|e| e.target == *target)
                    && !fs::mount_targets()?
                        .iter()
                        .any(|p| p == target || p.starts_with(target)))
            }
            Locator::Evidence { .. } => self.evidence_absent(r, context),
        }
    }

    fn absent_step(
        &self,
        record: &Record,
        context: &Context,
        cursor: &mut Option<String>,
        budget: DiscoveryBudget,
    ) -> Result<Option<bool>, ProtocolError> {
        if matches!(record.locator, Locator::Evidence { .. }) {
            self.evidence_absent_step(record, context, cursor, budget)
        } else {
            self.absent(record, context).map(Some)
        }
    }

    fn discover(&self, context: &Context) -> Result<Discovery, ProtocolError> {
        let mut cursor = DiscoveryCursor::default();
        loop {
            if let Some(result) =
                self.discover_host_step(context, &mut cursor, DiscoveryBudget::default())?
            {
                return Ok(result);
            }
        }
    }

    fn discover_step(
        &self,
        context: &Context,
        cursor: &mut DiscoveryCursor,
        budget: DiscoveryBudget,
    ) -> Result<Option<Discovery>, ProtocolError> {
        self.discover_host_step(context, cursor, budget)
    }

    fn validate(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
    ) -> Result<(), ProtocolError> {
        if r.source_owner_reason() == Some("stored_storage_record_invalid") {
            return Err(blocked("stored_storage_record_invalid"));
        }
        if r.artifact.protected {
            return Err(blocked("explicitly_protected"));
        }
        if context.protected_resources.contains(&r.resource_key)
            || context.protected_ancestors.contains(&r.resource_key)
        {
            return Err(blocked("protected_shared_data"));
        }
        if context.leased_resources.contains(&r.resource_key)
            || context.leased_ancestors.contains(&r.resource_key)
        {
            return Err(blocked("active_lease"));
        }
        if matches!(r.locator, Locator::Evidence { .. }) {
            for alias in &r.private_aliases {
                let (device, inode) = fs::identity(alias)?;
                let key = format!("fs:{device}:{inode}");
                if context.protected_ancestors.contains(&key) {
                    return Err(blocked("protected_shared_data"));
                }
                if context.leased_ancestors.contains(&key) {
                    return Err(blocked("active_lease"));
                }
            }
        }
        for alias in &r.private_aliases {
            for path in alias.ancestors() {
                if let Ok((device, inode)) = fs::identity(path) {
                    if context
                        .protected_resources
                        .contains(&format!("fs:{device}:{inode}"))
                    {
                        return Err(blocked("protected_shared_data"));
                    }
                    if context
                        .leased_resources
                        .contains(&format!("fs:{device}:{inode}"))
                    {
                        return Err(blocked("active_lease"));
                    }
                }
            }
        }
        if context.leased_artifacts.contains(&r.artifact.artifact_id) {
            return Err(blocked("active_lease"));
        }
        match &r.locator {
            Locator::Directory {
                path,
                device,
                inode,
                root,
                git_root,
            } => {
                let native_backup =
                    native_backup_generation(path, root, r.artifact.kind, &self.config);
                validate_path(path, &self.config, native_backup, &self.database)?;
                if !path.starts_with(root) {
                    return Err(blocked("outside_registered_root"));
                }
                if fs::identity(path)? != (*device, *inode) {
                    return Err(blocked("identity_changed"));
                }
                if native_backup {
                    self.native_backup_root_identity(r, root, context)?;
                    if path
                        .join("retained-sources.json")
                        .symlink_metadata()
                        .is_ok()
                    {
                        return Err(blocked("legacy_sources_retained"));
                    }
                    if path
                        .join("retirement-pending.json")
                        .symlink_metadata()
                        .is_ok()
                    {
                        return Err(blocked("activation_acceptance_pending"));
                    }
                    fs::validate_native_snapshot(path)?;
                }
                if r.artifact.kind == api::Kind::Backup && !context.discovering {
                    self.validate_backup_floor(r, context)?;
                }
                self.validate_mount_consumers(r, context, selected)?;
                if context
                    .current_paths
                    .iter()
                    .any(|p| path.starts_with(p) || p.starts_with(path))
                {
                    return Err(blocked("current_deployment"));
                }
                if context
                    .active_worktrees
                    .iter()
                    .any(|p| path.starts_with(p) || p.starts_with(path))
                {
                    return Err(blocked("active_worktree"));
                }
                if processes_reference(&r.private_aliases)? {
                    return Err(blocked("active_process"));
                }
                let measured = if context.discovering {
                    context
                        .discovery_measurements
                        .get(path)
                        .cloned()
                        .map(Ok)
                        .unwrap_or_else(|| fs::measure(path))?
                } else {
                    fs::measure(path)?
                };
                if measured.protected_metadata && generated_kind(r.artifact.kind) {
                    return Err(blocked("protected_source_or_credentials"));
                }
                if !context.discovering
                    && matches!(
                        r.artifact.kind,
                        api::Kind::BuildOutput
                            | api::Kind::DependencyCache
                            | api::Kind::BackingDirectory
                    )
                    && r.last_activity_signature
                        != format!(
                            "{}:{}:{}",
                            measured.newest_modified_ns, measured.bytes, measured.entries
                        )
                {
                    return Err(blocked("activity_changed"));
                }
                if measured.nested_git && r.artifact.kind != api::Kind::Worktree {
                    return Err(blocked("nested_repository"));
                }
                if r.artifact.kind == api::Kind::Worktree {
                    self.validate_worktree(r, path, git_root.as_deref(), context)?;
                } else if let Some(git) = git_root {
                    let git = context
                        .repositories
                        .iter()
                        .find(|repository| repository.root == *git)
                        .and_then(|repository| {
                            context
                                .worktrees
                                .iter()
                                .filter(|(id, worktree)| {
                                    id == &repository.id && path.starts_with(worktree)
                                })
                                .max_by_key(|(_, worktree)| worktree.components().count())
                                .map(|(_, worktree)| worktree.as_path())
                        })
                        .unwrap_or(git);
                    if !git_bytes(
                        git,
                        &[
                            "ls-files",
                            "-z",
                            "--",
                            path.to_str().ok_or_else(|| blocked("non_utf8_path"))?,
                        ],
                        context,
                    )?
                    .is_empty()
                    {
                        return Err(blocked("tracked_source"));
                    }
                }
                let allowed_mount = r
                    .artifact
                    .dependencies
                    .iter()
                    .any(|id| selected.contains(id));
                if fs::mount_targets()?
                    .iter()
                    .any(|p| p == path || p.starts_with(path))
                    && !allowed_mount
                {
                    return Err(blocked("mounted_path"));
                }
                Ok(())
            }
            Locator::Docker { .. } => self.validate_docker(r, context, selected),
            Locator::Mount { .. } => self.validate_mount(r, context, selected),
            Locator::Evidence { .. } => self.validate_evidence(r, context),
        }
    }

    fn remove(
        &self,
        r: &Record,
        context: &Context,
        selected: &[String],
        job_id: &str,
    ) -> Result<(), ProtocolError> {
        self.validate(r, context, selected)?;
        match &r.locator {
            Locator::Directory {
                path,
                device,
                inode,
                root,
                git_root,
            } => {
                if r.artifact.kind == api::Kind::Worktree {
                    let git = git_root
                        .as_deref()
                        .ok_or_else(|| blocked("git_owner_unknown"))?;
                    git_bytes(
                        git,
                        &[
                            "worktree",
                            "remove",
                            "--",
                            path.to_str().ok_or_else(|| blocked("non_utf8_path"))?,
                        ],
                        context,
                    )
                    .map_err(|_| unavailable("worktree_removal_failed"))?;
                    if path.exists() {
                        return Err(unavailable("worktree_removal_unverified"));
                    }
                    Ok(())
                } else if native_backup_generation(path, root, r.artifact.kind, &self.config) {
                    fs::remove_native_snapshot(
                        path,
                        (*device, *inode),
                        self.native_backup_root_identity(r, root, context)?,
                    )
                } else {
                    fs::remove_tree(
                        path,
                        (*device, *inode),
                        false,
                        generated_kind(r.artifact.kind),
                    )
                }
            }
            Locator::Docker { .. } => self.remove_docker(r),
            Locator::Mount { .. } => self.remove_mount(r, job_id),
            Locator::Evidence { .. } => self.remove_evidence(r, context),
        }
    }

    fn legacy_group(
        &self,
        id: &str,
        records: &[Record],
        context: &Context,
    ) -> Result<Vec<String>, ProtocolError> {
        if !context.observed.contains_key(id) {
            return Err(blocked("legacy_identity_not_registered"));
        }
        let owned = records
            .iter()
            .filter(|r| r.owner_deployment.as_deref() == Some(id))
            .collect::<Vec<_>>();
        if owned.is_empty() {
            return Err(blocked("legacy_inventory_unavailable"));
        }
        let ids = owned
            .iter()
            .map(|r| r.artifact.artifact_id.clone())
            .collect::<Vec<_>>();
        for r in owned {
            self.validate(r, context, &ids)?;
        }
        Ok(ids)
    }
}

pub fn validate_root(
    path: &Path,
    config: &Config,
    database: &crate::database::Database,
) -> Result<(), ProtocolError> {
    validate_path(path, config, false, database)
}

pub fn validate_configured_root(
    path: &Path,
    kind: api::Kind,
    config: &Config,
    database: &crate::database::Database,
) -> Result<(), ProtocolError> {
    validate_path(
        path,
        config,
        kind == api::Kind::Backup && path == config.state_dir.join("cutover"),
        database,
    )
}

fn native_backup_generation(path: &Path, root: &Path, kind: api::Kind, config: &Config) -> bool {
    kind == api::Kind::Backup
        && root == config.state_dir.join("cutover")
        && path.parent() == Some(root)
}

pub(super) fn validate_path(
    path: &Path,
    config: &Config,
    native_backup: bool,
    database: &crate::database::Database,
) -> Result<(), ProtocolError> {
    if !path.is_absolute()
        || path.components().any(|p| {
            matches!(
                p,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(blocked("invalid_storage_path"));
    }
    if path.components().count() < 4 {
        return Err(blocked("protected_root"));
    }
    let postgres_data = database
        .data_directory()
        .ok_or_else(|| unavailable("postgres_data_binding_unavailable"))?;
    let postgres_root = postgres_data
        .parent()
        .ok_or_else(|| unavailable("postgres_data_binding_unavailable"))?;
    if path.starts_with(postgres_root) || postgres_root.starts_with(path) {
        return Err(blocked("protected_system_data"));
    }
    let excluded = [
        "/etc", "/usr", "/bin", "/sbin", "/boot", "/dev", "/proc", "/sys", "/run",
    ];
    if excluded.iter().any(|p| path.starts_with(p))
        || path.starts_with("/var/lib/docker")
        || path.starts_with("/var/lib/devcoordinator2-postgres")
        || Path::new("/var/lib/devcoordinator2-postgres").starts_with(path)
        || (path.starts_with(&config.state_dir) && !native_backup)
        || config.state_dir.starts_with(path)
    {
        return Err(blocked("protected_system_data"));
    }
    if path.components().any(|p|matches!(p,std::path::Component::Normal(s) if s==".git" || fs::protected_metadata_name(s))) {return Err(blocked("protected_source_or_credentials"));}
    Ok(())
}

fn generated_kind(kind: api::Kind) -> bool {
    matches!(
        kind,
        api::Kind::BuildOutput | api::Kind::DependencyCache | api::Kind::Unknown
    )
}

pub(super) fn candidate(
    kind: api::Kind,
    effect: api::Effect,
    name: String,
    locator: Locator,
    context: &Context,
) -> Result<Record, ProtocolError> {
    let encoded = json(&locator)?;
    let fingerprint = hash(encoded.as_bytes());
    let id = stable_id("sa", encoded.as_bytes());
    Ok(Record {
        update_sequence: 0,
        artifact: api::Artifact {
            artifact_id: id.clone(),
            revision: 1,
            repository_id: None,
            repository_name: None,
            group_id: None,
            group_name: None,
            accounting_id: hash(id.as_bytes()),
            name: name.chars().take(180).collect(),
            kind,
            effect,
            ownership: "unknown".into(),
            filesystem_id: None,
            allocated_bytes: None,
            last_used_at_ms: None,
            observed_since_ms: context.now_ms,
            verified_at_ms: Some(context.now_ms),
            eligible_at_ms: None,
            safety: api::Safety::NeedsReview,
            reasons: Vec::new(),
            protected: false,
            deletable: false,
            automatic_eligible: false,
            dependencies: Vec::new(),
            aliases: Vec::new(),
            removed_at_ms: None,
        },
        locator,
        resource_key: id,
        ancestor_keys: Vec::new(),
        fingerprint,
        last_activity_signature: String::new(),
        owner_deployment: None,
        discovered_effect: effect,
        disposal_approved: false,
        disposal_reason: None,
        blockers: Vec::new(),
        source_owner_observation: None,
        recovery_lineage: None,
        recovery_verified: false,
        recovery_created_at_ms: None,
        private_aliases: Vec::new(),
        retention_eligible: false,
        ignored_review: None,
    })
}

pub(super) fn git_bytes(
    root: &Path,
    args: &[&str],
    context: &Context,
) -> Result<Vec<u8>, ProtocolError> {
    git_bytes_with_stderr(root, args, context).map(|(stdout, _)| stdout)
}

pub(super) fn git_bytes_with_stderr(
    root: &Path,
    args: &[&str],
    context: &Context,
) -> Result<(Vec<u8>, Vec<u8>), ProtocolError> {
    let repository = context
        .repositories
        .iter()
        .find(|r| {
            r.root == root
                || context
                    .worktrees
                    .iter()
                    .any(|(id, p)| id == &r.id && p == root)
        })
        .ok_or_else(|| blocked("git_execution_owner_unverified"))?;
    let uid = context
        .manual_caller_uid
        .or(Some(repository.execution_uid))
        .filter(|uid| *uid != 0)
        .ok_or_else(|| blocked("git_execution_owner_unverified"))?;
    if context.manual_caller_uid.is_some() {
        let gid = crate::systemd::primary_gid(uid)
            .map_err(|_| blocked("git_execution_owner_unverified"))?;
        let resolved = crate::repository::resolve_worktree(root, Some((uid, gid)))
            .map_err(|_| blocked("git_baseline_unverified"))?;
        if resolved.worktree_root != root || resolved.repository_root != repository.root {
            return Err(blocked("git_baseline_unverified"));
        }
    }
    let mut arguments = vec![
        OsString::from("--no-optional-locks"),
        OsString::from("-C"),
        root.as_os_str().to_owned(),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("core.hooksPath=/dev/null"),
    ];
    arguments.extend(args.iter().map(OsString::from));
    let program = if unsafe { libc::geteuid() } == 0 {
        let gid = crate::systemd::primary_gid(uid)
            .map_err(|_| blocked("git_execution_owner_unverified"))?;
        let mut prefix = vec![
            format!("--reuid={uid}").into(),
            format!("--regid={gid}").into(),
            "--init-groups".into(),
            "--reset-env".into(),
            "--".into(),
            "/usr/bin/git".into(),
        ];
        prefix.extend(arguments);
        arguments = prefix;
        "/usr/bin/setpriv"
    } else {
        if unsafe { libc::geteuid() } != uid {
            return Err(blocked("git_execution_owner_unverified"));
        }
        "/usr/bin/git"
    };
    let output = run_bounded(Path::new(program), &arguments, Duration::from_secs(30))
        .ok_or_else(|| unavailable("git_observation_unavailable"))?;
    if !output.status.success() || output.truncated {
        return Err(blocked("git_baseline_unverified"));
    }
    Ok((output.stdout, output.stderr))
}

pub(super) fn processes_reference(paths: &[PathBuf]) -> Result<bool, ProtocolError> {
    if paths.is_empty() {
        return Ok(false);
    }
    let entries =
        std::fs::read_dir("/proc").map_err(|_| unavailable("process_inventory_unavailable"))?;
    let own = std::process::id();
    for entry in entries {
        let entry = entry.map_err(|_| unavailable("process_inventory_unavailable"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own {
            continue;
        }
        let root = entry.path();
        let mut links = vec![root.join("cwd"), root.join("exe")];
        match std::fs::read_dir(root.join("fd")) {
            Ok(fd) => {
                for item in fd {
                    match item {
                        Ok(item) => links.push(item.path()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return Err(unavailable("process_inventory_incomplete")),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(unavailable("process_inventory_incomplete")),
        }
        for link in links {
            match std::fs::read_link(link) {
                Ok(target) if paths.iter().any(|p| target == *p || target.starts_with(p)) => {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                    ) => {}
                Err(_) => return Err(unavailable("process_inventory_incomplete")),
            }
        }
        match std::fs::read_to_string(root.join("maps")) {
            Ok(maps) => {
                for line in maps.lines() {
                    if let Some(position) = line.find('/') {
                        let path = Path::new(&line[position..]);
                        if paths.iter().any(|p| path == p || path.starts_with(p)) {
                            return Ok(true);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(unavailable("process_inventory_incomplete")),
        }
    }
    Ok(false)
}
