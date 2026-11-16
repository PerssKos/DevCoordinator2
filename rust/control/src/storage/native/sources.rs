use super::{
    Backend, HostBackend, candidate, git_bytes, git_bytes_with_stderr, processes_reference,
    validate_path,
};
use crate::database::OptionalExtension;
use crate::storage::{
    blocked, db_error, fs, hash, invalid,
    model::{Context, Discovery, IgnoredReview, IgnoredReviewFile, Locator, Record},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
#[cfg(target_os = "linux")]
use rustix::fs::{self as unix, Mode, OFlags};
#[cfg(target_os = "linux")]
use sha2::Digest;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MAX_IGNORED_REVIEW_FILES: usize = 128;
const MAX_IGNORED_REVIEW_BYTES: u64 = 8 * 1024 * 1024;

impl HostBackend {
    pub(super) fn validate_intent(
        &self,
        r: &Record,
        repository_id: &str,
        now_ms: u64,
        reviewed_ignored_files: Option<&[api::IgnoredFileReview]>,
    ) -> Result<(String, Option<IgnoredReview>), ProtocolError> {
        let recognized = r.artifact.ownership == "recognized";
        let cleanup_owned_source = r.artifact.ownership == "cleanup_owned"
            && r.disposal_approved
            && r.artifact.effect == api::Effect::SourceWorktree;
        if r.artifact.kind != api::Kind::Worktree
            || (!recognized && !cleanup_owned_source)
            || r.discovered_effect != api::Effect::SourceWorktree
        {
            return Err(blocked("disposal_intent_not_recognized"));
        }
        if r.artifact.repository_id.as_deref() != Some(repository_id) {
            return Err(blocked("ownership_conflict"));
        }
        let repository_id = repository_id.to_owned();
        let owner = self
            .database
            .read(move |connection| {
                Ok(connection
                    .query_row(
                        "SELECT display_name,root_path FROM repositories WHERE repository_id=$1 AND archived_at IS NULL",
                        [&repository_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                PathBuf::from(row.get::<_, String>(1)?),
                            ))
                        },
                    )
                    .optional()?)
            })
            .map_err(db_error)?
            .ok_or_else(|| invalid("repository_not_registered"))?;
        let Locator::Directory {
            path,
            device,
            inode,
            root,
            git_root,
        } = &r.locator
        else {
            return Err(blocked("worktree_locator_invalid"));
        };
        let normal_absolute = |value: &Path| {
            value.is_absolute()
                && value
                    .components()
                    .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        };
        if !normal_absolute(path) || !normal_absolute(root) {
            return Err(blocked("invalid_storage_path"));
        }
        if !path.starts_with(root) {
            return Err(blocked("outside_registered_root"));
        }
        let Some(git_root) = git_root else {
            return Err(blocked("git_owner_unknown"));
        };
        if git_root != &owner.1 {
            return Err(blocked("ownership_conflict"));
        }
        if path == git_root {
            return Err(blocked("canonical_checkout"));
        }
        // Keep the existing system-data and metadata protections while
        // avoiding a recursive measurement of the candidate tree.
        validate_path(path, &self.config, false, &self.database)?;
        fs::identity(root)?;
        fs::identity(git_root)?;
        let identity = fs::identity(path)?;
        if identity != (*device, *inode) {
            return Err(blocked("identity_changed"));
        }
        let ignored_review = reviewed_ignored_files
            .map(|files| self.review_ignored_files(path, identity, files))
            .transpose()?;

        let resource = r.resource_key.clone();
        let ancestors = crate::storage::json(&r.ancestor_keys)?;
        let lease_resource = resource.clone();
        let lease_ancestors = ancestors.clone();
        let artifact_id = r.artifact.artifact_id.clone();
        let protected = r.artifact.protected;
        let safety = self
            .database
            .read(move |connection| {
                let shared_protection = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM storage_artifacts p WHERE p.removed_at_ms IS NULL AND p.projection_protected AND (p.projection_resource_key=$1 OR p.projection_resource_key IN (SELECT value FROM jsonb_array_elements_text($2::jsonb)) OR p.projection_ancestors @> jsonb_build_array($1)))",
                    crate::database::params![resource, ancestors],
                    |row| row.get::<_, bool>(0),
                )?;
                let active_lease = connection.query_row(
                    "WITH owned(artifact_id) AS (SELECT $1::text UNION SELECT related.artifact_id FROM storage_artifacts related WHERE related.removed_at_ms IS NULL AND (related.projection_resource_key=$2 OR related.projection_resource_key IN (SELECT value FROM jsonb_array_elements_text($3::jsonb)) OR related.projection_ancestors @> jsonb_build_array($2))) SELECT EXISTS(SELECT 1 FROM owned JOIN storage_leases lease ON lease.projection_artifact_ids @> jsonb_build_array(owned.artifact_id) WHERE lease.projection_expires_at_ms>$4::bigint)",
                    crate::database::params![artifact_id, lease_resource, lease_ancestors, now_ms as i64],
                    |row| row.get::<_, bool>(0),
                )?;
                Ok((shared_protection, active_lease))
            })
            .map_err(db_error)?;
        if protected || safety.0 {
            return Err(blocked(if protected {
                "explicitly_protected"
            } else {
                "protected_shared_data"
            }));
        }
        if safety.1 {
            return Err(blocked("active_lease"));
        }
        Ok((owner.0, ignored_review))
    }

    pub(super) fn native_backup_root_identity(
        &self,
        r: &Record,
        root: &Path,
        context: &Context,
    ) -> Result<(u64, u64), ProtocolError> {
        let registered = context
            .roots
            .iter()
            .find(|configured| {
                configured.path == root
                    && configured.root.kind == api::Kind::Backup
                    && (context.discovering
                        || r.artifact.group_id.as_ref() == Some(&configured.root.root_id))
            })
            .ok_or_else(|| blocked("native_backup_root_unverified"))?;
        let expected = (registered.device, registered.inode);
        if fs::identity(root)? != expected {
            return Err(blocked("configured_root_identity_changed"));
        }
        Ok(expected)
    }

    pub(super) fn discover_retained_backing(
        &self,
        context: &Context,
        out: &mut Discovery,
    ) -> Result<(), ProtocolError> {
        // Retiring fstab must not make the still-existing backing directory
        // disappear from inventory. The registry keeps that exact identity.
        let rows=self.database.read(|c| {
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
        if checked.last_activity_signature != r.last_activity_signature {
            return Err(blocked("backup_changed"));
        }
        if checked.blockers.iter().any(|b| b == "backup_in_progress") {
            return Err(blocked("backup_in_progress"));
        }
        let scope = r.artifact.repository_id.clone().unwrap_or_default();
        let floor=self.database.read(move|c|{
            use crate::database::OptionalExtension;
            let value=c.query_row("SELECT policy_json FROM storage_policies WHERE scope=$1 OR scope='' ORDER BY CASE WHEN scope=$1 THEN 0 ELSE 1 END LIMIT 1",[scope],|r|r.get::<_,String>(0)).optional()?;
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
            other.artifact.group_id = r.artifact.group_id.clone();
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

    fn ignored_review_path_allowed(path: &Path) -> bool {
        let components = path
            .components()
            .map(|component| match component {
                Component::Normal(name) => name.to_string_lossy().into_owned(),
                _ => String::new(),
            })
            .collect::<Vec<_>>();
        if components.iter().any(String::is_empty)
            || components.iter().any(|name| {
                name == ".git"
                    || fs::protected_metadata_name(name.as_ref())
                    || matches!(
                        name.as_str(),
                        "chat"
                            | "chats"
                            | "chat-history"
                            | "chat_history"
                            | "conversations"
                            | "messages"
                    )
            })
        {
            return false;
        }
        match components.as_slice() {
            [name] if name == ".devcoordinator.toml" => true,
            [root, test, name]
                if root == ".devcoordinator"
                    && test == "test"
                    && matches!(name.as_str(), "history.json" | "evidence.json") =>
            {
                true
            }
            [root, test, current, name]
                if root == ".devcoordinator"
                    && test == "test"
                    && current == "current"
                    && matches!(
                        name.as_str(),
                        "check-plan.json"
                            | "check-report.json"
                            | "container-lifecycle.json"
                            | "env"
                            | "summary.json"
                    ) =>
            {
                true
            }
            [root, ..] if root == ".devcoordinator" => false,
            _ => true,
        }
    }

    #[cfg(target_os = "linux")]
    fn hash_ignored_file(
        root: &Path,
        relative: &Path,
        remaining: u64,
    ) -> Result<IgnoredReviewFile, ProtocolError> {
        let full = root.join(relative);
        let parent = fs::open_directory(
            full.parent()
                .ok_or_else(|| blocked("ignored_review_path_invalid"))?,
        )?;
        let descriptor = unix::openat(
            &parent,
            full.file_name()
                .ok_or_else(|| blocked("ignored_review_path_invalid"))?,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| blocked("ignored_review_file_unavailable"))?;
        let mut file = File::from(descriptor);
        let before =
            unix::fstat(&file).map_err(|_| unavailable("ignored_review_file_unavailable"))?;
        if unix::FileType::from_raw_mode(before.st_mode) != unix::FileType::RegularFile {
            return Err(blocked("ignored_review_file_not_regular"));
        }
        let size = u64::try_from(before.st_size)
            .ok()
            .filter(|size| *size <= remaining)
            .ok_or_else(|| blocked("ignored_review_size_limit"))?;
        let mut digest = sha2::Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut read = 0_u64;
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| unavailable("ignored_review_file_unavailable"))?;
            if count == 0 {
                break;
            }
            read = read.saturating_add(count as u64);
            if read > remaining {
                return Err(blocked("ignored_review_size_limit"));
            }
            digest.update(&buffer[..count]);
        }
        let after =
            unix::fstat(&file).map_err(|_| unavailable("ignored_review_file_unavailable"))?;
        if before.st_dev != after.st_dev
            || before.st_ino != after.st_ino
            || before.st_size != after.st_size
            || before.st_mtime != after.st_mtime
            || before.st_mtime_nsec != after.st_mtime_nsec
            || read != size
        {
            return Err(blocked("ignored_review_file_changed"));
        }
        Ok(IgnoredReviewFile {
            path: relative.to_owned(),
            device: before.st_dev as u64,
            inode: before.st_ino as u64,
            size,
            sha256: crate::storage::hex(&digest.finalize()),
        })
    }

    #[cfg(target_os = "linux")]
    fn review_ignored_files(
        &self,
        root: &Path,
        worktree_identity: (u64, u64),
        files: &[api::IgnoredFileReview],
    ) -> Result<IgnoredReview, ProtocolError> {
        if files.len() > MAX_IGNORED_REVIEW_FILES {
            return Err(blocked("ignored_review_file_limit"));
        }
        let mut seen = BTreeSet::new();
        let mut reviewed = Vec::with_capacity(files.len());
        let mut total = 0_u64;
        for file in files {
            if file.sha256.len() != 64 || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(blocked("ignored_review_digest_invalid"));
            }
            let relative = Path::new(&file.path);
            if !relative.is_relative()
                || relative
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
                || !Self::ignored_review_path_allowed(relative)
                || !seen.insert(relative.to_owned())
            {
                return Err(blocked("ignored_review_path_invalid"));
            }
            let entry = Self::hash_ignored_file(
                root,
                relative,
                MAX_IGNORED_REVIEW_BYTES.saturating_sub(total),
            )?;
            if entry.sha256 != file.sha256.to_ascii_lowercase() {
                return Err(blocked("ignored_review_digest_mismatch"));
            }
            total = total.saturating_add(entry.size);
            reviewed.push(entry);
        }
        Ok(IgnoredReview {
            worktree_device: worktree_identity.0,
            worktree_inode: worktree_identity.1,
            files: reviewed,
            total_bytes: total,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn review_ignored_files(
        &self,
        _root: &Path,
        _worktree_identity: (u64, u64),
        _files: &[api::IgnoredFileReview],
    ) -> Result<IgnoredReview, ProtocolError> {
        Err(unavailable("ignored_review_unsupported"))
    }

    #[cfg(target_os = "linux")]
    fn validate_ignored_review(
        &self,
        path: &Path,
        record: &Record,
        ignored: &[u8],
        context: &Context,
    ) -> Result<(), ProtocolError> {
        let Some(review) = record.ignored_review.as_ref() else {
            return Err(blocked("unreviewed_ignored_files"));
        };
        let Locator::Directory { device, inode, .. } = &record.locator else {
            return Err(blocked("unreviewed_ignored_files"));
        };
        if review.worktree_device != *device || review.worktree_inode != *inode {
            return Err(blocked("ignored_review_identity_changed"));
        }
        let listed = ignored
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                std::str::from_utf8(entry)
                    .map(PathBuf::from)
                    .map_err(|_| blocked("ignored_review_listing_invalid"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let expected = review
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<BTreeSet<_>>();
        if listed != expected {
            return Err(blocked("unreviewed_ignored_files"));
        }
        if review.total_bytes > MAX_IGNORED_REVIEW_BYTES {
            return Err(blocked("ignored_review_size_limit"));
        }
        if review
            .files
            .iter()
            .any(|file| !Self::ignored_review_path_allowed(&file.path))
        {
            return Err(blocked("ignored_review_forbidden_path"));
        }
        if listed.is_empty() && !review.files.is_empty() {
            return Err(blocked("unreviewed_ignored_files"));
        }
        let mut remaining = MAX_IGNORED_REVIEW_BYTES;
        let mut total = 0_u64;
        for file in &review.files {
            let current = Self::hash_ignored_file(path, &file.path, remaining)?;
            remaining = remaining.saturating_sub(current.size);
            total = total.saturating_add(current.size);
            if current.device != file.device
                || current.inode != file.inode
                || current.size != file.size
                || current.sha256 != file.sha256
            {
                return Err(blocked("ignored_review_file_changed"));
            }
        }
        if total != review.total_bytes {
            return Err(blocked("ignored_review_file_changed"));
        }
        if review
            .files
            .iter()
            .any(|file| file.path.starts_with(".devcoordinator/test"))
            && !crate::storage::evidence::protected_runs(&self.database, path)?.is_empty()
        {
            return Err(blocked("protected_evidence"));
        }
        if context
            .active_worktrees
            .iter()
            .any(|worktree| worktree == path)
        {
            return Err(blocked("active_worktree"));
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn validate_ignored_review(
        &self,
        _path: &Path,
        _record: &Record,
        _ignored: &[u8],
        _context: &Context,
    ) -> Result<(), ProtocolError> {
        Err(unavailable("ignored_review_unsupported"))
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
        let measured = context
            .discovery_measurements
            .get(path)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| fs::measure(path));
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
        if let Some(review) = context.ignored_reviews.get(&r.resource_key) {
            r.ignored_review = Some(review.clone());
        }
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
            if e.message == "git_execution_owner_unverified"
                && let Some(git_root) = git
            {
                r.source_owner_observation =
                    Some(super::super::model::SourceOwnerObservation::Unavailable {
                        resource_key: r.resource_key.clone(),
                        fingerprint: r.fingerprint.clone(),
                        git_root: git_root.to_path_buf(),
                        origin: context
                            .manual_caller_uid
                            .map_or(super::super::model::SourceOwnerOrigin::Registered, |uid| {
                                super::super::model::SourceOwnerOrigin::LocalManual { uid }
                            }),
                    });
            } else {
                r.blockers.push(e.message);
            }
        }
        Ok(r)
    }

    pub(super) fn validate_worktree(
        &self,
        record: &Record,
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
        let (ignored, ignored_stderr) = git_bytes_with_stderr(
            path,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
            ],
            context,
        )?;
        if !ignored_stderr.is_empty() {
            return Err(blocked("ignored_files_unverified"));
        }
        if !ignored.is_empty()
            || record
                .ignored_review
                .as_ref()
                .is_some_and(|review| !review.files.is_empty())
        {
            self.validate_ignored_review(path, record, &ignored, context)?;
        }
        if !ignored.is_empty() && record.ignored_review.is_none() {
            return Err(blocked("unreviewed_ignored_files"));
        }
        let head = String::from_utf8(git_bytes(path, &["rev-parse", "HEAD"], context)?)
            .map_err(|_| blocked("git_baseline_unverified"))?;
        let branch = String::from_utf8(git_bytes(
            path,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            context,
        )?)
        .map_err(|_| blocked("git_baseline_unverified"))?;
        let branch = branch.trim();
        let upstream = if branch == "HEAD" {
            None
        } else {
            let remote_key = format!("branch.{branch}.remote");
            let merge_key = format!("branch.{branch}.merge");
            let remote = String::from_utf8(git_bytes(
                path,
                &["config", "--get", "--default=", &remote_key],
                context,
            )?)
            .map_err(|_| blocked("git_baseline_unverified"))?;
            let merge = String::from_utf8(git_bytes(
                path,
                &["config", "--get", "--default=", &merge_key],
                context,
            )?)
            .map_err(|_| blocked("git_baseline_unverified"))?;
            let remote = remote.trim();
            let merge = merge.trim();
            if remote.is_empty() && merge.is_empty() {
                None
            } else if remote == "."
                || remote.chars().any(char::is_whitespace)
                || !merge.starts_with("refs/heads/")
                || merge.chars().any(char::is_whitespace)
            {
                return Err(blocked("git_baseline_unverified"));
            } else {
                git_bytes(path, &["remote", "get-url", "--", remote], context)?;
                Some((remote.to_owned(), merge.to_owned()))
            }
        };
        let remote = if let Some((remote_name, remote_ref)) = upstream.as_ref() {
            git_bytes(
                path,
                &["ls-remote", "--exit-code", "--", remote_name, remote_ref],
                context,
            )?
        } else {
            git_bytes(path, &["ls-remote", "origin", "HEAD"], context)?
        };
        let remote = String::from_utf8(remote).map_err(|_| blocked("git_baseline_unverified"))?;
        let mut fields = remote.split_whitespace();
        let remote_head = fields
            .next()
            .filter(|h| {
                (h.len() == 40 || h.len() == 64) && h.chars().all(|c| c.is_ascii_hexdigit())
            })
            .ok_or_else(|| blocked("git_baseline_unverified"))?;
        if let Some((_, remote_ref)) = upstream.as_ref()
            && (fields.next() != Some(remote_ref) || fields.next().is_some())
        {
            return Err(blocked("git_baseline_unverified"));
        }
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
        r.recovery_verified = false;
        let Locator::Directory { path, root, .. } = &r.locator else {
            return Ok(());
        };
        if path
            .join("retirement-pending.json")
            .symlink_metadata()
            .is_ok()
        {
            r.blockers.push("activation_acceptance_pending".into());
            return Ok(());
        }
        if super::native_backup_generation(path, root, r.artifact.kind, &self.config) {
            self.native_backup_root_identity(r, root, context)?;
            fs::validate_native_snapshot(path)?;
        }
        if path.join("installation-snapshot.json").is_file() {
            let repository = r
                .artifact
                .repository_id
                .as_ref()
                .or_else(|| context.repositories.first().map(|r| &r.id))
                .ok_or_else(|| blocked("backup_owner_unverified"))?;
            let observation = crate::planning_backup::inspect_configured(
                self.database.persistence().config(),
                &crate::planning_backup::InspectRequest {
                    transaction_dir: path.clone(),
                    repository_id: repository.clone(),
                    task_ids: Vec::new(),
                    include_identities: false,
                },
            )
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
                r.recovery_verified = false;
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
