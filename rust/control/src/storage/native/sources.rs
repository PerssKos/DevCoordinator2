use super::{Backend, HostBackend, candidate, git_bytes, processes_reference};
use crate::storage::{
    blocked, fs, hash,
    model::{Context, Discovery, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

impl HostBackend {
    pub(super) fn discover_retained_backing(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        // Retiring fstab must not make the still-existing backing directory
        // disappear from inventory. The registry keeps that exact identity.
        let rows=self.database.call(|c| {
            let mut q=c.prepare("SELECT record_json FROM storage_artifacts WHERE kind='backing_directory' AND removed_at_ms IS NULL")?;
            Ok(q.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?)
        }).map_err(crate::storage::db_error)?;
        for value in rows {
            let old: Record = crate::storage::parse(&value)?;
            if out
                .records
                .iter()
                .any(|r| r.artifact.artifact_id == old.artifact.artifact_id)
                || context
                    .scan_repository_id
                    .as_ref()
                    .is_some_and(|repo| old.artifact.repository_id.as_ref() != Some(repo))
            {
                continue;
            }
            let Locator::Directory {
                path,
                root,
                device,
                inode,
                ..
            } = &old.locator
            else {
                continue;
            };
            if fs::absent(path).unwrap_or(false) {
                continue;
            }
            if fs::identity(path).ok() != Some((*device, *inode)) {
                out.coverage_gaps.push("backing_identity_unverified".into());
                continue;
            }
            let mut fresh = self.directory_candidate(
                path,
                root,
                None,
                api::Kind::BackingDirectory,
                true,
                context,
            )?;
            fresh.owner_deployment = old.owner_deployment;
            fresh.artifact.repository_id = old.artifact.repository_id;
            fresh.artifact.repository_name = old.artifact.repository_name;
            fresh.artifact.group_id = old.artifact.group_id;
            fresh.artifact.group_name = old.artifact.group_name;
            fresh.artifact.name = old.artifact.name;
            fresh.artifact.dependencies = old.artifact.dependencies;
            out.records.push(fresh);
        }
        Ok(())
    }

    pub(super) fn validate_backup_floor(
        &self,
        r: &Record,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let Locator::Directory { path, root, .. } = &r.locator else {
            return Err(blocked("backup_unverified"));
        };
        let mut checked = r.clone();
        checked.recovery_verified = false;
        self.observe_backup(&mut checked, context)?;
        if !checked.recovery_verified {
            return Err(blocked("backup_unverified"));
        }
        if checked.blockers.iter().any(|b| b == "backup_in_progress") {
            return Err(blocked("backup_in_progress"));
        }
        let scope = r.artifact.repository_id.clone().unwrap_or_default();
        let floor=self.database.call(move|c|{
            use rusqlite::OptionalExtension;
            let value=c.query_row("SELECT policy_json FROM storage_policies WHERE scope=?1 OR scope='' ORDER BY CASE WHEN scope=?1 THEN 0 ELSE 1 END LIMIT 1",[scope],|r|r.get::<_,String>(0)).optional()?;
            Ok(value)
        }).map_err(crate::storage::db_error)?.map(|v|crate::storage::parse::<api::Policy>(&v).map(|p|p.minimum_verified_backups)).transpose()?.unwrap_or(2);
        let mut newer = 0u32;
        for entry in
            std::fs::read_dir(root).map_err(|_| unavailable("backup_inventory_unavailable"))?
        {
            let entry = entry.map_err(|_| unavailable("backup_inventory_unavailable"))?;
            if entry.path() == *path || !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let (device, inode) = fs::identity(&entry.path())?;
            let mut other = candidate(
                api::Kind::Backup,
                api::Effect::RecoveryCopy,
                entry.file_name().to_string_lossy().into_owned(),
                Locator::Directory {
                    path: entry.path(),
                    device,
                    inode,
                    root: root.clone(),
                    git_root: None,
                },
                context,
            )?;
            other.artifact.repository_id = r.artifact.repository_id.clone();
            if self.observe_backup(&mut other, context).is_ok()
                && other.recovery_verified
                && other.recovery_lineage == checked.recovery_lineage
                && other.recovery_created_at_ms >= checked.recovery_created_at_ms
            {
                newer += 1;
            }
            if newer >= floor {
                return Ok(());
            }
        }
        Err(blocked("required_recovery"))
    }

    pub(super) fn discover_sources(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        let mut visited = BTreeSet::new();
        let mut generated = Vec::new();
        for repo in context.repositories.iter().filter(|repo| {
            context
                .scan_repository_id
                .as_ref()
                .is_none_or(|id| id == &repo.id)
        }) {
            let mut roots = vec![repo.root.clone()];
            roots.extend(
                context
                    .worktrees
                    .iter()
                    .filter(|(id, _)| id == &repo.id)
                    .map(|(_, p)| p.clone()),
            );
            for root in roots {
                if !visited.insert(root.clone()) {
                    continue;
                }
                if let Ok(f) = fs::filesystem(&root, context.now_ms, "Project storage") {
                    out.filesystems.push(f);
                }
                if let Err(e) = discover_generated(&root, &root, 0, &mut generated) {
                    out.coverage_gaps.push(e.message);
                }
                for (path, kind, recognized) in generated.drain(..) {
                    match self.directory_candidate(
                        &path,
                        &root,
                        Some(&root),
                        kind,
                        recognized,
                        context,
                    ) {
                        Ok(mut r) => {
                            r.artifact.repository_id = Some(repo.id.clone());
                            r.artifact.repository_name = Some(repo.name.clone());
                            out.records.push(r);
                        }
                        Err(_) => out
                            .coverage_gaps
                            .push("generated_output_observation_incomplete".into()),
                    }
                }
                if root != repo.root {
                    match self.directory_candidate(
                        &root,
                        &root,
                        Some(&repo.root),
                        api::Kind::Worktree,
                        true,
                        context,
                    ) {
                        Ok(mut r) => {
                            r.artifact.repository_id = Some(repo.id.clone());
                            r.artifact.repository_name = Some(repo.name.clone());
                            r.artifact.name = root
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            out.records.push(r);
                        }
                        Err(_) => out
                            .coverage_gaps
                            .push("worktree_observation_incomplete".into()),
                    }
                }
            }
        }
        for root in context.roots.iter().filter(|root| {
            context
                .scan_repository_id
                .as_ref()
                .is_none_or(|id| root.root.repository_id.as_ref() == Some(id))
        }) {
            if fs::identity(&root.path).ok() != Some((root.device, root.inode)) {
                out.coverage_gaps
                    .push("configured_root_identity_changed".into());
                continue;
            }
            if let Ok(f) = fs::filesystem(&root.path, context.now_ms, &root.root.label) {
                out.filesystems.push(f);
            }
            let children = std::fs::read_dir(&root.path)
                .map_err(|_| unavailable("configured_root_unavailable"))?;
            for child in children {
                let child = child.map_err(|_| unavailable("configured_root_incomplete"))?;
                if !child.file_type().is_ok_and(|k| k.is_dir()) {
                    continue;
                }
                let recognized = root.root.kind != api::Kind::Unknown;
                let git = context
                    .repositories
                    .iter()
                    .find(|repo| child.path().starts_with(&repo.root))
                    .map(|repo| repo.root.as_path());
                match self.directory_candidate(
                    &child.path(),
                    &root.path,
                    git,
                    root.root.kind,
                    recognized,
                    context,
                ) {
                    Ok(mut r) => {
                        r.artifact.repository_id = root.root.repository_id.clone();
                        r.artifact.group_id = Some(root.root.root_id.clone());
                        r.artifact.group_name = Some(root.root.label.clone());
                        r.artifact.repository_name = context
                            .repositories
                            .iter()
                            .find(|p| Some(&p.id) == r.artifact.repository_id.as_ref())
                            .map(|p| p.name.clone());
                        if r.artifact.kind == api::Kind::Backup {
                            self.observe_backup(&mut r, context)?;
                        }
                        out.records.push(r);
                    }
                    Err(_) => out
                        .coverage_gaps
                        .push("configured_artifact_observation_incomplete".into()),
                }
            }
        }
        if out.coverage_gaps.is_empty() {
            out.complete_kinds.extend([
                api::Kind::BuildOutput,
                api::Kind::DependencyCache,
                api::Kind::Worktree,
                api::Kind::Backup,
                api::Kind::Unknown,
            ]);
        }
        Ok(())
    }

    pub(super) fn directory_candidate(
        &self,
        path: &Path,
        root: &Path,
        git: Option<&Path>,
        kind: api::Kind,
        recognized: bool,
        context: &Context,
    ) -> Result<Record, ProtocolError> {
        let measured = fs::measure(path);
        let (device, inode) = fs::identity(path)?;
        let effect = if !recognized {
            api::Effect::Unknown
        } else {
            match kind {
                api::Kind::Worktree => api::Effect::SourceWorktree,
                api::Kind::Backup => api::Effect::RecoveryCopy,
                api::Kind::BackingDirectory => api::Effect::PermanentData,
                _ => api::Effect::Rebuildable,
            }
        };
        let name = path
            .strip_prefix(root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(path.file_name().map(Path::new).unwrap_or(path))
            .to_string_lossy()
            .into_owned();
        let mut r = candidate(
            kind,
            effect,
            name,
            Locator::Directory {
                path: path.into(),
                device,
                inode,
                root: root.into(),
                git_root: git.map(Path::to_path_buf),
            },
            context,
        )?;
        r.resource_key = format!("fs:{device}:{inode}");
        r.artifact.filesystem_id = Some(format!("fs-{device}"));
        match &measured {
            Ok(m) => {
                r.artifact.allocated_bytes = Some(m.bytes);
                r.last_activity_signature =
                    format!("{}:{}:{}", m.newest_modified_ns, m.bytes, m.entries);
            }
            Err(error) => r.blockers.push(error.message.clone()),
        }
        r.private_aliases.push(path.into());
        r.artifact.ownership = if recognized { "recognized" } else { "unknown" }.into();
        if !recognized {
            r.blockers.push("ownership_unknown".into());
        }
        if measured.as_ref().is_ok_and(|m| m.nested_git) && kind != api::Kind::Worktree {
            r.blockers.push("nested_repository".into());
        }
        if kind == api::Kind::Backup {
            r.blockers.push("backup_unverified".into());
        }
        if kind == api::Kind::Worktree {
            r.blockers.push("disposal_not_authorized".into());
        }
        if let Err(e) = self.validate(&r, context, &[]) {
            r.blockers.push(e.message);
        }
        Ok(r)
    }

    pub(super) fn validate_worktree(
        &self,
        path: &Path,
        main: Option<&Path>,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let main = main.ok_or_else(|| blocked("git_owner_unknown"))?;
        if path == main || context.repositories.iter().any(|r| r.root == path) {
            return Err(blocked("canonical_checkout"));
        }
        if context
            .current_paths
            .iter()
            .any(|p| p.starts_with(path) || path.starts_with(p))
        {
            return Err(blocked("current_deployment"));
        }
        if !git_bytes(
            path,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            context,
        )?
        .is_empty()
        {
            return Err(blocked("dirty_worktree"));
        }
        if !git_bytes(
            path,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
            ],
            context,
        )?
        .is_empty()
        {
            return Err(blocked("unreviewed_ignored_files"));
        }
        let head = String::from_utf8(git_bytes(path, &["rev-parse", "HEAD"], context)?)
            .map_err(|_| blocked("git_baseline_unverified"))?;
        let remote = String::from_utf8(git_bytes(main, &["ls-remote", "origin", "HEAD"], context)?)
            .map_err(|_| blocked("git_baseline_unverified"))?;
        let remote_head = remote
            .split_whitespace()
            .next()
            .filter(|h| h.len() == 40 || h.len() == 64)
            .ok_or_else(|| blocked("git_baseline_unverified"))?;
        git_bytes(main, &["cat-file", "-e", remote_head], context)?;
        git_bytes(
            main,
            &["merge-base", "--is-ancestor", head.trim(), remote_head],
            context,
        )
        .map_err(|_| blocked("unique_worktree_commits"))?;
        if processes_reference(&[path.into()])? {
            return Err(blocked("active_worktree"));
        }
        Ok(())
    }

    pub(super) fn observe_backup(
        &self,
        r: &mut Record,
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let Locator::Directory { path, root, .. } = &r.locator else {
            return Ok(());
        };
        if path.join("installation-snapshot.json").is_file() {
            let repository = r
                .artifact
                .repository_id
                .as_ref()
                .or_else(|| context.repositories.first().map(|r| &r.id))
                .ok_or_else(|| blocked("backup_owner_unverified"))?;
            let observation =
                crate::planning_backup::inspect(&crate::planning_backup::InspectRequest {
                    transaction_dir: path.clone(),
                    repository_id: repository.clone(),
                    task_ids: Vec::new(),
                    include_identities: false,
                })
                .map_err(|_| blocked("backup_unverified"))?;
            if observation.provenance != "recorded_hash_matched" {
                return Ok(());
            }
            let modified = std::fs::metadata(path.join("installation-snapshot.json"))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| u64::try_from(d.as_millis()).ok());
            r.recovery_created_at_ms = modified;
            r.recovery_lineage = Some(format!(
                "authority:{}",
                hash(root.to_string_lossy().as_bytes())
            ));
            r.recovery_verified = modified.is_some();
            r.last_activity_signature = observation.backup_sha256;
            if r.recovery_verified {
                r.blockers.retain(|s| s != "backup_unverified");
            }
            if matches!(
                observation.snapshot_status.as_str(),
                "prepared" | "rolling_back"
            ) {
                r.blockers.push("backup_in_progress".into());
            }
            return Ok(());
        }
        let manifest = path.join("backup-manifest.json");
        let Ok(metadata) = std::fs::symlink_metadata(&manifest) else {
            return Ok(());
        };
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&manifest).map_err(|_| unavailable("backup_manifest_unavailable"))?,
        )
        .map_err(|_| blocked("backup_manifest_invalid"))?;
        let Some(lineage) = value
            .get("lineage")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty() && s.len() < 128)
        else {
            return Ok(());
        };
        let Some(created) = value
            .get("created_at_ms")
            .and_then(|v| v.as_u64())
            .filter(|t| *t <= context.now_ms)
        else {
            return Ok(());
        };
        let Some(files) = value
            .get("files")
            .and_then(|v| v.as_array())
            .filter(|v| !v.is_empty() && v.len() <= 256)
        else {
            return Ok(());
        };
        for file in files {
            let Some(name) = file.get("file").and_then(|v| v.as_str()) else {
                return Ok(());
            };
            let relative = Path::new(name);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|p| !matches!(p, std::path::Component::Normal(_)))
            {
                return Ok(());
            }
            let Some(expected) = file
                .get("sha256")
                .and_then(|v| v.as_str())
                .filter(|s| s.len() == 64)
            else {
                return Ok(());
            };
            let full = path.join(relative);
            if !std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_file()) {
                return Ok(());
            }
            use sha2::Digest;
            use std::io::Read;
            let parent = fs::open_directory(
                full.parent()
                    .ok_or_else(|| blocked("backup_manifest_invalid"))?,
            )?;
            let fd = rustix::fs::openat(
                &parent,
                full.file_name()
                    .ok_or_else(|| blocked("backup_manifest_invalid"))?,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|_| blocked("backup_file_changed"))?;
            let mut input = std::fs::File::from(fd);
            let before = input
                .metadata()
                .map_err(|_| unavailable("backup_metadata_unavailable"))?;
            let mut digest = sha2::Sha256::new();
            let mut bytes = [0; 64 * 1024];
            loop {
                let n = input
                    .read(&mut bytes)
                    .map_err(|_| unavailable("backup_read_failed"))?;
                if n == 0 {
                    break;
                }
                digest.update(&bytes[..n]);
            }
            if crate::storage::hex(&digest.finalize()) != expected
                || input.metadata().ok().and_then(|m| m.modified().ok()) != before.modified().ok()
            {
                return Ok(());
            }
        }
        r.recovery_lineage = Some(format!(
            "{}:{lineage}",
            hash(root.to_string_lossy().as_bytes())
        ));
        r.recovery_created_at_ms = Some(created);
        r.recovery_verified = true;
        r.blockers.retain(|s| s != "backup_unverified");
        r.last_activity_signature =
            hash(&std::fs::read(manifest).map_err(|_| unavailable("backup_manifest_unavailable"))?);
        Ok(())
    }
}

fn discover_generated(
    path: &Path,
    root: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, api::Kind, bool)>,
) -> Result<(), ProtocolError> {
    if depth > 8 {
        return Ok(());
    }
    if out.len() > 10_000 {
        return Err(blocked("source_discovery_limit"));
    }
    for entry in std::fs::read_dir(path).map_err(|_| unavailable("source_discovery_unavailable"))? {
        let entry = entry.map_err(|_| unavailable("source_discovery_incomplete"))?;
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if matches!(
            n.as_ref(),
            ".git"
                | ".devcoordinator"
                | ".devcoordinator-test"
                | ".ssh"
                | "sessions"
                | "archived_sessions"
        ) {
            continue;
        }
        let candidate = match n.as_ref() {
            "target" => Some((
                api::Kind::BuildOutput,
                path.join("Cargo.toml").is_file()
                    && entry.path().join(".rustc_info.json").is_file(),
            )),
            "node_modules" => Some((
                api::Kind::DependencyCache,
                path.join("package.json").is_file()
                    && (entry.path().join(".package-lock.json").is_file()
                        || entry.path().join(".modules.yaml").is_file()),
            )),
            ".venv" | ".venv-v3" | "venv" => Some((
                api::Kind::DependencyCache,
                entry.path().join("pyvenv.cfg").is_file(),
            )),
            ".pytest_cache" | ".ruff_cache" => Some((
                api::Kind::DependencyCache,
                entry.path().join("CACHEDIR.TAG").is_file(),
            )),
            "dist" | "build" | "bazel-out" | ".next" => Some((api::Kind::BuildOutput, false)),
            _ => None,
        };
        if let Some((kind, recognized)) = candidate {
            out.push((entry.path(), kind, recognized));
            continue;
        }
        if entry.path() != root && entry.path().join(".git").exists() {
            continue;
        }
        discover_generated(&entry.path(), root, depth + 1, out)?;
    }
    Ok(())
}
