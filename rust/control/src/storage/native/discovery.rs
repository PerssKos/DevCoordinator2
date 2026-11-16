//! Scan continuations keep filesystem enumeration and measurement off request turns.
use super::HostBackend;
use crate::storage::{
    blocked, fs,
    model::{Context, Discovery},
    unavailable,
};
use devcoordinator2_api::{ProtocolError, storage as api};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct DiscoveryBudget {
    pub entries: usize,
    pub elapsed: Duration,
}
impl Default for DiscoveryBudget {
    fn default() -> Self {
        Self {
            entries: 256,
            elapsed: Duration::from_millis(25),
        }
    }
}

#[derive(Default)]
pub struct DiscoveryCursor {
    host: Option<HostScan>,
    pub(super) complete: bool,
}

struct HostScan {
    context: Context,
    result: Discovery,
    sources: SourceCursor,
    phase: u8,
    evidence: super::evidence::EvidenceCursor,
    finalize_index: usize,
}

#[derive(Clone)]
struct SourceRoot {
    path: PathBuf,
    git: Option<PathBuf>,
    repository_id: Option<String>,
    repository_name: Option<String>,
    kind: Option<api::Kind>,
    group_id: Option<String>,
    group_name: Option<String>,
    worktree: bool,
}
struct Candidate {
    root: SourceRoot,
    path: PathBuf,
    kind: api::Kind,
    recognized: bool,
}
type SourceTraversal = (SourceRoot, Vec<(PathBuf, std::fs::ReadDir, usize)>);
struct SourceCursor {
    roots: VecDeque<SourceRoot>,
    current: Option<SourceTraversal>,
    pending: Option<(Candidate, fs::MeasurementCursor)>,
    candidates: usize,
    had_gap: bool,
}

impl SourceCursor {
    fn new(context: &Context) -> Self {
        let mut roots = VecDeque::new();
        let mut visited = BTreeSet::new();
        for repository in &context.repositories {
            if context
                .scan_repository_id
                .as_ref()
                .is_some_and(|id| id != &repository.id)
            {
                continue;
            }
            for path in std::iter::once(&repository.root).chain(
                context
                    .worktrees
                    .iter()
                    .filter(|(id, _)| id == &repository.id)
                    .map(|(_, path)| path),
            ) {
                if !visited.insert(path.clone()) {
                    continue;
                }
                roots.push_back(SourceRoot {
                    path: path.clone(),
                    git: Some(repository.root.clone()),
                    repository_id: Some(repository.id.clone()),
                    repository_name: Some(repository.name.clone()),
                    kind: None,
                    group_id: None,
                    group_name: None,
                    worktree: path != &repository.root,
                });
            }
        }
        for root in &context.roots {
            if context
                .scan_repository_id
                .as_ref()
                .is_some_and(|id| root.root.repository_id.as_ref() != Some(id))
            {
                continue;
            }
            roots.push_back(SourceRoot {
                path: root.path.clone(),
                git: None,
                repository_id: root.root.repository_id.clone(),
                repository_name: context
                    .repositories
                    .iter()
                    .find(|p| Some(&p.id) == root.root.repository_id.as_ref())
                    .map(|p| p.name.clone()),
                kind: Some(root.root.kind),
                group_id: Some(root.root.root_id.clone()),
                group_name: Some(root.root.label.clone()),
                worktree: false,
            });
        }
        Self {
            roots,
            current: None,
            pending: None,
            candidates: 0,
            had_gap: false,
        }
    }

    fn gap(&mut self, out: &mut Discovery, message: &str) {
        self.had_gap = true;
        out.coverage_gaps.push(message.into());
    }

    fn prepare(&mut self, candidate: Candidate, out: &mut Discovery) {
        self.candidates += 1;
        match fs::MeasurementCursor::new(&candidate.path) {
            Ok(measurement) => self.pending = Some((candidate, measurement)),
            Err(_) => self.gap(out, "source_discovery_incomplete"),
        }
    }

    fn step(
        &mut self,
        backend: &HostBackend,
        context: &mut Context,
        out: &mut Discovery,
        budget: DiscoveryBudget,
    ) -> Result<bool, ProtocolError> {
        let started = Instant::now();
        let mut entries = 0;
        loop {
            if entries >= budget.entries.max(1)
                || (entries > 0 && started.elapsed() >= budget.elapsed)
            {
                return Ok(false);
            }
            if let Some((candidate, measurement)) = self.pending.as_mut() {
                let measured = measurement.step(
                    budget.entries.saturating_sub(entries).max(1),
                    budget.elapsed.saturating_sub(started.elapsed()),
                );
                match measured {
                    Ok(None) => return Ok(false),
                    Ok(Some(measured)) => {
                        context
                            .discovery_measurements
                            .insert(candidate.path.clone(), measured);
                        let result = backend.directory_candidate(
                            &candidate.path,
                            &candidate.root.path,
                            candidate.root.git.as_deref(),
                            candidate.kind,
                            candidate.recognized,
                            context,
                        );
                        context.discovery_measurements.remove(&candidate.path);
                        match result {
                            Ok(mut record) => {
                                record.artifact.repository_id =
                                    candidate.root.repository_id.clone();
                                record.artifact.repository_name =
                                    candidate.root.repository_name.clone();
                                record.artifact.group_id = candidate.root.group_id.clone();
                                record.artifact.group_name = candidate.root.group_name.clone();
                                if record.artifact.kind == api::Kind::Backup
                                    && backend.observe_backup(&mut record, context).is_err()
                                {
                                    self.had_gap = true;
                                    out.coverage_gaps
                                        .push("backup_observation_incomplete".into());
                                }
                                out.records.push(record);
                            }
                            Err(_) => {
                                self.had_gap = true;
                                out.coverage_gaps.push("source_discovery_incomplete".into());
                            }
                        }
                    }
                    Err(_) => {
                        self.had_gap = true;
                        out.coverage_gaps
                            .push("source_measurement_incomplete".into());
                    }
                }
                self.pending = None;
                // A candidate includes bounded native Git/process checks. It is
                // the last operation in this turn even when enumeration is cheap.
                return Ok(false);
            }
            if self.current.is_none() {
                let Some(root) = self.roots.pop_front() else {
                    if !self.had_gap {
                        out.complete_kinds.extend([
                            api::Kind::BuildOutput,
                            api::Kind::DependencyCache,
                            api::Kind::Worktree,
                            api::Kind::Backup,
                            api::Kind::Unknown,
                        ]);
                    }
                    return Ok(true);
                };
                entries += 1;
                if let Some(configured) = context
                    .roots
                    .iter()
                    .find(|r| Some(&r.root.root_id) == root.group_id.as_ref())
                    && fs::identity(&root.path).ok() != Some((configured.device, configured.inode))
                {
                    self.gap(out, "configured_root_identity_changed");
                    continue;
                }
                if let Ok(filesystem) = fs::filesystem(
                    &root.path,
                    context.now_ms,
                    root.group_name.as_deref().unwrap_or("Project storage"),
                ) {
                    out.filesystems.push(filesystem);
                }
                let directory = match std::fs::read_dir(&root.path) {
                    Ok(directory) => directory,
                    Err(_) => {
                        self.gap(out, "source_discovery_unavailable");
                        continue;
                    }
                };
                let worktree = root.worktree;
                self.current = Some((root.clone(), vec![(root.path.clone(), directory, 0)]));
                if worktree {
                    self.prepare(
                        Candidate {
                            path: root.path.clone(),
                            root,
                            kind: api::Kind::Worktree,
                            recognized: true,
                        },
                        out,
                    );
                }
                continue;
            }
            let (root, stack) = self.current.as_mut().expect("selected source root");
            let Some((parent, directory, depth)) = stack.last_mut() else {
                self.current = None;
                continue;
            };
            let Some(entry) = directory.next() else {
                stack.pop();
                continue;
            };
            entries += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    self.had_gap = true;
                    out.coverage_gaps.push("source_discovery_incomplete".into());
                    continue;
                }
            };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if root.kind.is_none()
                && matches!(
                    name.as_ref(),
                    ".git"
                        | ".devcoordinator"
                        | ".devcoordinator-test"
                        | ".ssh"
                        | "sessions"
                        | "archived_sessions"
                )
            {
                continue;
            }
            let selected = if let Some(kind) = root.kind {
                Some((kind, kind != api::Kind::Unknown))
            } else {
                match name.as_ref() {
                    "target" => Some((
                        api::Kind::BuildOutput,
                        parent.join("Cargo.toml").is_file()
                            && entry.path().join(".rustc_info.json").is_file(),
                    )),
                    "node_modules" => Some((
                        api::Kind::DependencyCache,
                        parent.join("package.json").is_file()
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
                    "dist" | "build" | "bazel-out" | ".next" => {
                        Some((api::Kind::BuildOutput, false))
                    }
                    _ => None,
                }
            };
            if let Some((kind, recognized)) = selected {
                let mut selected_root = root.clone();
                if selected_root.git.is_none() {
                    selected_root.git = context
                        .repositories
                        .iter()
                        .find(|repository| entry.path().starts_with(&repository.root))
                        .map(|repository| repository.root.clone());
                }
                if self.candidates >= 10_000 {
                    return Err(blocked("source_discovery_limit"));
                }
                self.prepare(
                    Candidate {
                        root: selected_root,
                        path: entry.path(),
                        kind,
                        recognized,
                    },
                    out,
                );
                continue;
            }
            if *depth >= 8 || entry.path().join(".git").exists() {
                continue;
            }
            let child_depth = *depth + 1;
            match std::fs::read_dir(entry.path()) {
                Ok(directory) => stack.push((entry.path(), directory, child_depth)),
                Err(_) => {
                    self.had_gap = true;
                    out.coverage_gaps.push("source_discovery_incomplete".into());
                }
            }
        }
    }
}

impl HostBackend {
    pub(super) fn discover_host_step(
        &self,
        context: &Context,
        cursor: &mut DiscoveryCursor,
        budget: DiscoveryBudget,
    ) -> Result<Option<Discovery>, ProtocolError> {
        if cursor.complete {
            return Err(unavailable("discovery_already_complete"));
        }
        let state = cursor.host.get_or_insert_with(|| {
            let mut context = context.clone();
            context.discovering = true;
            HostScan {
                sources: SourceCursor::new(&context),
                result: Discovery {
                    repository_id: context.scan_repository_id.clone(),
                    ..Discovery::default()
                },
                context,
                phase: 0,
                evidence: super::evidence::EvidenceCursor::default(),
                finalize_index: 0,
            }
        });
        match state.phase {
            0 => {
                match self.container_rows() {
                    Ok(rows) => state.context.docker_snapshot = Some(std::sync::Arc::new(rows)),
                    Err(error) => {
                        state.context.docker_unavailable = true;
                        let category = match error.message.as_str() {
                            "docker_unavailable" => "invocation_failed",
                            "docker_observation_incomplete" => "command_or_output_incomplete",
                            "docker_metadata_invalid" => "metadata_invalid",
                            "docker_inventory_limit" => "inventory_limit",
                            _ => "other",
                        };
                        tracing::error!(
                            failure_category = category,
                            "native Docker container snapshot failed"
                        );
                    }
                };
                state.phase = 1;
            }
            1 => {
                if state
                    .sources
                    .step(self, &mut state.context, &mut state.result, budget)?
                {
                    state.phase = 2;
                }
            }
            2 => {
                if self.discover_evidence_step(
                    &state.context,
                    &mut state.result,
                    &mut state.evidence,
                    budget,
                )? {
                    state.phase = 3;
                }
            }
            3 => {
                if !state.context.repositories.is_empty() {
                    match self.discover_docker(&state.context, &mut state.result) {
                        Ok(()) => state.result.complete_kinds.extend([
                            api::Kind::Container,
                            api::Kind::Image,
                            api::Kind::Network,
                            api::Kind::Volume,
                            api::Kind::Mount,
                            api::Kind::BackingDirectory,
                        ]),
                        Err(_) => state
                            .result
                            .coverage_gaps
                            .push("docker_discovery_incomplete".into()),
                    }
                };
                state.phase = 4;
            }
            4 => {
                self.discover_retained_backing(&state.context, &mut state.result)?;
                state.phase = 5;
            }
            _ => {
                let started = Instant::now();
                let mut processed = 0;
                for record in state.result.records.iter_mut().skip(state.finalize_index) {
                    if processed >= 32 || (processed > 0 && started.elapsed() >= budget.elapsed) {
                        break;
                    }
                    let mut ancestors = BTreeSet::new();
                    for alias in &record.private_aliases {
                        for parent in alias
                            .ancestors()
                            .skip(usize::from(record.artifact.kind != api::Kind::Evidence))
                        {
                            if let Ok((device, inode)) = fs::identity(parent) {
                                ancestors.insert(format!("fs:{device}:{inode}"));
                            }
                        }
                    }
                    record.ancestor_keys = ancestors.into_iter().collect();
                    processed += 1;
                }
                state.finalize_index += processed;
                if state.finalize_index >= state.result.records.len() {
                    let mut filesystems = BTreeSet::new();
                    state
                        .result
                        .filesystems
                        .retain(|f| filesystems.insert(f.filesystem_id.clone()));
                    cursor.complete = true;
                    return Ok(Some(std::mem::take(&mut state.result)));
                }
            }
        }
        Ok(None)
    }
}
