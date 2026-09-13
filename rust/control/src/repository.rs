//! Git identity and registered-repository lifecycle.

use std::collections::HashMap;
use std::ffi::{CStr, OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use devcoordinator2_api::results::{
    RegisteredRepository, Repository, RepositoryList, RepositoryListRow, RepositoryPresentation,
    RepositoryStatus, Worktree,
};
use devcoordinator2_api::{ErrorCode, ProtocolError};
use rusqlite::{Connection, OptionalExtension, Row};
use time::{OffsetDateTime, macros::format_description};

use crate::database::{Database, DatabaseError};
use crate::ids;

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_POLL: Duration = Duration::from_millis(10);
const PROCESS_OUTPUT_CAP: usize = 64 * 1024;
const GIT_ERROR_CAP: usize = 512;
const EXEC_PATH: &str = "/usr/bin:/bin";

/// Canonical roots reported by Git for one worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeInfo {
    /// The canonical directory which owns the shared `.git` directory.
    pub repository_root: PathBuf,
    /// The canonical top-level directory of the selected worktree.
    pub worktree_root: PathBuf,
}

/// Query for runtime blockers owned outside the repository registry.
///
/// Persistent planning, release, and deployment blockers are checked atomically
/// by [`Registry`]. A test-lifecycle implementation can provide the additional
/// active-test check through this interface without making the repository
/// registry depend on that service. Implementations must be read-only.
pub trait ArchiveBlockerQuery: Send + Sync + 'static {
    fn first_blocker(
        &self,
        repository_id: &str,
        repository_root: &Path,
    ) -> Result<Option<String>, ProtocolError>;
}

#[derive(Debug, Default)]
pub struct NoAdditionalArchiveBlockers;

impl ArchiveBlockerQuery for NoAdditionalArchiveBlockers {
    fn first_blocker(
        &self,
        _repository_id: &str,
        _repository_root: &Path,
    ) -> Result<Option<String>, ProtocolError> {
        Ok(None)
    }
}

impl<F> ArchiveBlockerQuery for F
where
    F: Fn(&str, &Path) -> Result<Option<String>, ProtocolError> + Send + Sync + 'static,
{
    fn first_blocker(
        &self,
        repository_id: &str,
        repository_root: &Path,
    ) -> Result<Option<String>, ProtocolError> {
        self(repository_id, repository_root)
    }
}

/// Repository and worktree authority backed by the Coordinator database.
#[derive(Clone)]
pub struct Registry {
    database: Database,
    additional_archive_blockers: Arc<dyn ArchiveBlockerQuery>,
}

impl Registry {
    pub fn new(database: Database) -> Self {
        Self::with_archive_blockers(database, NoAdditionalArchiveBlockers)
    }

    pub fn with_archive_blockers<Q>(database: Database, blockers: Q) -> Self
    where
        Q: ArchiveBlockerQuery,
    {
        Self {
            database,
            additional_archive_blockers: Arc::new(blockers),
        }
    }

    /// Resolve and upsert a repository and its selected worktree.
    pub fn register(
        &self,
        path: &Path,
        caller_uid: u32,
        caller_gid: u32,
    ) -> Result<RegisteredRepository, ProtocolError> {
        validate_absolute_path(path)?;
        let info = resolve_worktree(path, Some((caller_uid, caller_gid)))?;
        let repository_id = ids::repository_id(&info.repository_root).map_err(id_error)?;
        let worktree_id = ids::worktree_id(&info.worktree_root).map_err(id_error)?;
        let root_path = path_text(&info.repository_root)?;
        let worktree_path = path_text(&info.worktree_root)?;
        let display_name = info
            .repository_root
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_owned();
        let now = timestamp()?;
        let transaction_repository_id = repository_id.clone();
        let transaction_worktree_id = worktree_id.clone();
        let transaction_root_path = root_path.clone();
        let transaction_worktree_path = worktree_path.clone();
        let transaction_display_name = display_name.clone();

        let outcome = self
            .database
            .transaction(move |connection| {
                let existing = connection
                    .query_row(
                        "SELECT archived_at, merged_into_repository_id \
                         FROM repositories WHERE repository_id=?1",
                        [&transaction_repository_id],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                            ))
                        },
                    )
                    .optional()?;
                if let Some((Some(_), merged_into_repository_id)) = existing.as_ref() {
                    return Ok(RegisterOutcome::Archived {
                        repository_id: transaction_repository_id,
                        merged_into_repository_id: merged_into_repository_id.clone(),
                    });
                }

                let newly_registered = existing.is_none();
                if newly_registered {
                    connection.execute(
                        "INSERT INTO repositories(\
                           repository_id,root_path,display_name,registered_at,\
                           registered_by_uid,last_seen_at\
                         ) VALUES(?1,?2,?3,?4,?5,?4)",
                        (
                            &transaction_repository_id,
                            &transaction_root_path,
                            &transaction_display_name,
                            &now,
                            i64::from(caller_uid),
                        ),
                    )?;
                } else {
                    connection.execute(
                        "UPDATE repositories SET last_seen_at=?1 WHERE repository_id=?2",
                        (&now, &transaction_repository_id),
                    )?;
                }

                let worktree_exists = connection
                    .query_row(
                        "SELECT 1 FROM worktrees WHERE worktree_id=?1",
                        [&transaction_worktree_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if worktree_exists {
                    connection.execute(
                        "UPDATE worktrees SET last_seen_at=?1 WHERE worktree_id=?2",
                        (&now, &transaction_worktree_id),
                    )?;
                } else {
                    connection.execute(
                        "INSERT INTO worktrees(\
                           worktree_id,repository_id,worktree_path,registered_at,last_seen_at\
                         ) VALUES(?1,?2,?3,?4,?4)",
                        (
                            &transaction_worktree_id,
                            &transaction_repository_id,
                            &transaction_worktree_path,
                            &now,
                        ),
                    )?;
                }
                Ok(RegisterOutcome::Registered(newly_registered))
            })
            .map_err(database_error)?;

        match outcome {
            RegisterOutcome::Registered(newly_registered) => Ok(RegisteredRepository {
                repository_id,
                worktree_id,
                root_path,
                worktree_path,
                display_name,
                registered: newly_registered,
            }),
            RegisterOutcome::Archived {
                repository_id,
                merged_into_repository_id,
            } => {
                let suffix = merged_into_repository_id
                    .map(|replacement| format!("; use {replacement}"))
                    .unwrap_or_default();
                Err(ProtocolError::new(
                    ErrorCode::RepositoryArchived,
                    format!("repository {repository_id} is archived{suffix}"),
                ))
            }
        }
    }

    pub fn list_repositories(
        &self,
        include_archived: bool,
    ) -> Result<RepositoryList, ProtocolError> {
        self.database
            .call(move |connection| {
                let repositories = {
                    let sql = if include_archived {
                        "SELECT repository_id,root_path,display_name,registered_at,last_seen_at,\
                                archived_at,archived_by_uid,archive_note,merged_into_repository_id \
                         FROM repositories ORDER BY display_name,repository_id"
                    } else {
                        "SELECT repository_id,root_path,display_name,registered_at,last_seen_at,\
                                archived_at,archived_by_uid,archive_note,merged_into_repository_id \
                         FROM repositories WHERE archived_at IS NULL \
                         ORDER BY display_name,repository_id"
                    };
                    let mut statement = connection.prepare(sql)?;
                    statement
                        .query_map([], repository_list_row)?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let included = repositories
                    .iter()
                    .map(|repository| repository.repository_id.as_str())
                    .collect::<std::collections::HashSet<_>>();
                let mut worktrees: HashMap<String, Vec<Worktree>> = HashMap::new();
                {
                    let mut statement = connection.prepare(
                        "SELECT worktree_id,repository_id,worktree_path \
                         FROM worktrees ORDER BY rowid",
                    )?;
                    let rows = statement.query_map([], |row| {
                        Ok((
                            row.get::<_, String>(1)?,
                            Worktree {
                                worktree_id: row.get(0)?,
                                worktree_path: row.get(2)?,
                            },
                        ))
                    })?;
                    for row in rows {
                        let (repository_id, worktree) = row?;
                        if included.contains(repository_id.as_str()) {
                            worktrees.entry(repository_id).or_default().push(worktree);
                        }
                    }
                }
                Ok(RepositoryList {
                    repositories: repositories
                        .into_iter()
                        .map(|mut repository| {
                            repository.worktrees = worktrees
                                .remove(&repository.repository_id)
                                .unwrap_or_default();
                            repository
                        })
                        .collect(),
                })
            })
            .map_err(database_error)
    }

    pub fn repository_status(
        &self,
        path: &Path,
        run_as: Option<(u32, u32)>,
    ) -> Result<RepositoryStatus, ProtocolError> {
        validate_absolute_path(path)?;
        let info = resolve_worktree(path, run_as).map_err(|_| {
            ProtocolError::new(
                ErrorCode::RepositoryNotFound,
                format!("no registered repository contains {}", path.display()),
            )
        })?;
        let repository_id = ids::repository_id(&info.repository_root).map_err(id_error)?;
        let worktree_id = ids::worktree_id(&info.worktree_root).map_err(id_error)?;
        let listed = self.list_repositories(true)?;
        let repository = listed
            .repositories
            .into_iter()
            .find(|repository| repository.repository_id == repository_id)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::RepositoryNotFound,
                    format!("no registered repository contains {}", path.display()),
                )
            })?;
        Ok(RepositoryStatus {
            repository_id: repository.repository_id,
            root_path: repository.root_path,
            display_name: repository.display_name,
            registered_at: repository.registered_at,
            last_seen_at: repository.last_seen_at,
            archived_at: repository.archived_at,
            archived_by_uid: repository.archived_by_uid,
            archive_note: repository.archive_note,
            merged_into_repository_id: repository.merged_into_repository_id,
            worktrees: repository.worktrees,
            worktree_id,
            current_test: None,
        })
    }

    pub fn registered_worktree_paths(&self) -> Result<Vec<PathBuf>, ProtocolError> {
        self.database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT w.worktree_path FROM worktrees w \
                     JOIN repositories r ON r.repository_id=w.repository_id \
                     WHERE r.archived_at IS NULL ORDER BY w.rowid",
                )?;
                Ok(statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect())
            })
            .map_err(database_error)
    }

    pub fn archive(
        &self,
        repository_id: &str,
        merged_into_repository_id: &str,
        note: &str,
        actor_uid: u32,
    ) -> Result<Repository, ProtocolError> {
        validate_repository_id(repository_id, "repository_id")?;
        validate_repository_id(merged_into_repository_id, "merged_into_repository_id")?;
        validate_note(note)?;
        if repository_id == merged_into_repository_id {
            return Err(archive_blocked("a repository cannot replace itself"));
        }

        let source = self
            .repository(repository_id)?
            .ok_or_else(|| archive_blocked("source and replacement repositories must exist"))?;
        let target = self
            .repository(merged_into_repository_id)?
            .ok_or_else(|| archive_blocked("source and replacement repositories must exist"))?;
        if target.archived_at.is_some() {
            return Err(archive_blocked("replacement repository must be active"));
        }
        if source.archived_at.is_some() {
            if source.merged_into_repository_id.as_deref() == Some(merged_into_repository_id) {
                return Ok(source);
            }
            return Err(archive_blocked(
                "repository is already archived with another replacement",
            ));
        }
        if let Some(blocker) = self.persistent_blocker(repository_id)? {
            return Err(archive_blocked(blocker));
        }
        if let Some(blocker) = self
            .additional_archive_blockers
            .first_blocker(repository_id, Path::new(&source.root_path))?
        {
            return Err(archive_blocked(blocker));
        }

        let repository_id = repository_id.to_owned();
        let replacement_id = merged_into_repository_id.to_owned();
        let note = note.to_owned();
        let now = timestamp()?;
        let outcome = self
            .database
            .transaction(move |connection| {
                let source = select_repository(connection, &repository_id)?;
                let target = select_repository(connection, &replacement_id)?;
                let (Some(source), Some(target)) = (source, target) else {
                    return Ok(ArchiveOutcome::Blocked(
                        "source and replacement repositories must exist".to_owned(),
                    ));
                };
                if target.archived_at.is_some() {
                    return Ok(ArchiveOutcome::Blocked(
                        "replacement repository must be active".to_owned(),
                    ));
                }
                if source.archived_at.is_some() {
                    return if source.merged_into_repository_id.as_deref()
                        == Some(replacement_id.as_str())
                    {
                        Ok(ArchiveOutcome::Repository(source))
                    } else {
                        Ok(ArchiveOutcome::Blocked(
                            "repository is already archived with another replacement".to_owned(),
                        ))
                    };
                }
                if let Some(blocker) = persistent_archive_blocker(connection, &repository_id)? {
                    return Ok(ArchiveOutcome::Blocked(blocker));
                }
                connection.execute(
                    "UPDATE repositories SET archived_at=?1,archived_by_uid=?2,\
                     archive_note=?3,merged_into_repository_id=?4 WHERE repository_id=?5",
                    (
                        &now,
                        i64::from(actor_uid),
                        &note,
                        &replacement_id,
                        &repository_id,
                    ),
                )?;
                connection.execute(
                    "INSERT INTO repository_events(\
                       repository_id,event,merged_into_repository_id,actor_uid,at,note\
                     ) VALUES(?1,'archived',?2,?3,?4,?5)",
                    (
                        &repository_id,
                        &replacement_id,
                        i64::from(actor_uid),
                        &now,
                        &note,
                    ),
                )?;
                Ok(ArchiveOutcome::Repository(
                    select_repository(connection, &repository_id)?
                        .expect("updated repository remains present"),
                ))
            })
            .map_err(database_error)?;
        match outcome {
            ArchiveOutcome::Repository(repository) => Ok(repository),
            ArchiveOutcome::Blocked(message) => Err(archive_blocked(message)),
        }
    }

    pub fn unarchive(
        &self,
        repository_id: &str,
        note: &str,
        actor_uid: u32,
    ) -> Result<Repository, ProtocolError> {
        validate_repository_id(repository_id, "repository_id")?;
        validate_note(note)?;
        let repository_id = repository_id.to_owned();
        let note = note.to_owned();
        let now = timestamp()?;
        let outcome = self
            .database
            .transaction(move |connection| {
                let Some(source) = select_repository(connection, &repository_id)? else {
                    return Ok(ArchiveOutcome::Blocked(
                        "repository does not exist".to_owned(),
                    ));
                };
                if source.archived_at.is_none() {
                    return Ok(ArchiveOutcome::Repository(source));
                }
                let checkout = std::fs::symlink_metadata(&source.root_path);
                if checkout.is_err()
                    || !checkout.is_ok_and(|metadata| metadata.file_type().is_dir())
                {
                    return Ok(ArchiveOutcome::Blocked(
                        "repository checkout must exist before unarchive".to_owned(),
                    ));
                }
                connection.execute(
                    "UPDATE repositories SET archived_at=NULL,archived_by_uid=NULL,\
                     archive_note=NULL,merged_into_repository_id=NULL WHERE repository_id=?1",
                    [&repository_id],
                )?;
                connection.execute(
                    "INSERT INTO repository_events(\
                       repository_id,event,merged_into_repository_id,actor_uid,at,note\
                     ) VALUES(?1,'unarchived',NULL,?2,?3,?4)",
                    (&repository_id, i64::from(actor_uid), &now, &note),
                )?;
                Ok(ArchiveOutcome::Repository(
                    select_repository(connection, &repository_id)?
                        .expect("updated repository remains present"),
                ))
            })
            .map_err(database_error)?;
        match outcome {
            ArchiveOutcome::Repository(repository) => Ok(repository),
            ArchiveOutcome::Blocked(message) => Err(archive_blocked(message)),
        }
    }

    pub fn update_presentation(
        &self,
        params: devcoordinator2_api::params::RepositoryPresentationUpdate,
        uid: u32,
    ) -> Result<RepositoryPresentation, ProtocolError> {
        validate_repository_id(&params.repository_id, "repository_id")?;
        let display_name = params.display_name.map(|name| name.trim().to_owned());
        if display_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control)
        }) {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "Repository name must contain 1 to 80 characters without control characters.",
            ));
        }
        let icons = [
            "folder",
            "code",
            "app-window",
            "world",
            "rocket",
            "database",
            "device-desktop",
            "device-mobile",
            "tools",
            "flask",
            "palette",
            "star",
            "plane",
            "book",
            "chart-bar",
            "shield",
        ];
        if params
            .icon
            .as_ref()
            .is_some_and(|icon| !icons.contains(&icon.as_str()))
        {
            return Err(ProtocolError::new(
                ErrorCode::ParamsInvalid,
                "Choose a supported repository icon.",
            ));
        }
        let result = RepositoryPresentation {
            repository_id: params.repository_id,
            display_name,
            icon: params.icon,
        };
        let saved = result.clone();
        let found = self.database.call(move |connection| {
            let transaction = connection.transaction()?;
            let active: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM repositories WHERE repository_id=?1 AND archived_at IS NULL)",
                [&saved.repository_id], |row| row.get(0),
            )?;
            if !active { return Ok(false); }
            if saved.display_name.is_none() && saved.icon.is_none() {
                transaction.execute("DELETE FROM repository_presentation WHERE repository_id=?1", [&saved.repository_id])?;
            } else {
                transaction.execute(
                    "INSERT INTO repository_presentation(repository_id,display_name,icon,updated_at,updated_by_uid) \
                     VALUES(?1,?2,?3,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?4) \
                     ON CONFLICT(repository_id) DO UPDATE SET display_name=excluded.display_name,icon=excluded.icon,updated_at=excluded.updated_at,updated_by_uid=excluded.updated_by_uid",
                    rusqlite::params![saved.repository_id, saved.display_name, saved.icon, uid],
                )?;
            }
            transaction.commit()?;
            Ok(true)
        }).map_err(database_error)?;
        if !found {
            return Err(repository_not_found(
                "repository is not registered or is archived",
            ));
        }
        Ok(result)
    }

    fn repository(&self, repository_id: &str) -> Result<Option<Repository>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| select_repository(connection, &repository_id))
            .map_err(database_error)
    }

    fn persistent_blocker(&self, repository_id: &str) -> Result<Option<String>, ProtocolError> {
        let repository_id = repository_id.to_owned();
        self.database
            .call(move |connection| persistent_archive_blocker(connection, &repository_id))
            .map_err(database_error)
    }
}

enum RegisterOutcome {
    Registered(bool),
    Archived {
        repository_id: String,
        merged_into_repository_id: Option<String>,
    },
}

enum ArchiveOutcome {
    Repository(Repository),
    Blocked(String),
}

/// Resolve a path to the canonical common repository and worktree roots.
///
/// When the daemon is root and a non-root physical caller is supplied, Git is
/// invoked through `setpriv` with that caller's UID, GID, and supplementary
/// groups. Git never parses caller-owned configuration as root in that case.
pub fn resolve_worktree(
    path: &Path,
    run_as: Option<(u32, u32)>,
) -> Result<WorktreeInfo, ProtocolError> {
    let probe = if path.is_dir() {
        path
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    };
    if !probe.is_dir() {
        return Err(repository_not_found(format!(
            "not a directory: {}",
            path.display()
        )));
    }
    let output = run_git(probe, run_as).map_err(|error| {
        repository_not_found(format!(
            "git invocation failed: {}",
            truncate_text(&error.to_string(), GIT_ERROR_CAP)
        ))
    })?;
    parse_git_result(output)
}

fn run_git(probe: &Path, run_as: Option<(u32, u32)>) -> io::Result<GitOutput> {
    run_git_arguments(
        probe,
        run_as,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
        GIT_TIMEOUT,
    )
}

pub(crate) fn test_repository_source(
    probe: &Path,
    run_as: (u32, u32),
) -> Option<devcoordinator2_api::results::TestRepositorySource> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut checkout = probe.canonicalize().ok()?;
    let mut visited = std::collections::HashSet::new();
    for depth in 0..8 {
        if !visited.insert(checkout.clone()) {
            return None;
        }
        if depth > 0 {
            let output = run_git_arguments(
                &checkout,
                Some(run_as),
                &["rev-parse", "--show-toplevel"],
                deadline.checked_duration_since(Instant::now())?,
            )
            .ok()?;
            let root = if output.status.success() {
                output
            } else {
                run_git_arguments(
                    &checkout,
                    Some(run_as),
                    &["rev-parse", "--absolute-git-dir"],
                    deadline.checked_duration_since(Instant::now())?,
                )
                .ok()?
            };
            if !root.status.success()
                || root.stdout.len() > 4096
                || Path::new(std::str::from_utf8(&root.stdout).ok()?.trim())
                    .canonicalize()
                    .ok()?
                    != checkout
            {
                return None;
            }
        }
        let output = run_git_arguments(
            &checkout,
            Some(run_as),
            &[
                "config",
                "--local",
                "--no-includes",
                "--get",
                "remote.origin.url",
            ],
            deadline.checked_duration_since(Instant::now())?,
        )
        .ok()?;
        if !output.status.success() || output.stdout.len() > 2048 {
            return None;
        }
        let remote = std::str::from_utf8(&output.stdout).ok()?.trim();
        if let Some(source) = repository_source_from_remote(remote) {
            return Some(source);
        }
        checkout = local_repository_origin(remote, &checkout)?
            .canonicalize()
            .ok()?;
    }
    None
}

/// Resolve presentation independently of retained test runs, using the root's
/// physical owner for the existing bounded, credential-free Git discovery.
pub(crate) fn repository_source_for_root(
    root: &Path,
) -> Option<devcoordinator2_api::results::TestRepositorySource> {
    use std::os::unix::fs::MetadataExt;

    let metadata = root.metadata().ok()?;
    test_repository_source(root, (metadata.uid(), metadata.gid()))
}

fn local_repository_origin(remote: &str, checkout: &Path) -> Option<PathBuf> {
    if remote.starts_with("file://") {
        let parsed = reqwest::Url::parse(remote).ok()?;
        if parsed.query().is_some()
            || parsed.fragment().is_some()
            || !matches!(parsed.host_str(), None | Some("localhost"))
        {
            return None;
        }
        return parsed.to_file_path().ok();
    }
    if remote.is_empty() || remote.contains(':') || remote.contains('\n') {
        return None;
    }
    Some(checkout.join(remote))
}

fn repository_source_from_remote(
    remote: &str,
) -> Option<devcoordinator2_api::results::TestRepositorySource> {
    use sha2::{Digest, Sha256};

    let address = if remote.contains("://") {
        remote.to_owned()
    } else {
        let (authority, path) = remote.split_once(':')?;
        if authority.contains('/') || !authority.contains('@') {
            return None;
        }
        format!("ssh://{authority}/{path}")
    };
    let parsed = reqwest::Url::parse(&address).ok()?;
    if !matches!(parsed.scheme(), "https" | "http" | "ssh" | "git")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let repository = parsed.path().trim_matches('/');
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if repository.len() > 512
        || repository.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.len() > 128
                || !part.chars().all(|character| {
                    character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        })
    {
        return None;
    }
    let name = repository.rsplit('/').next()?.to_owned();
    let port = parsed
        .port()
        .filter(|port| !(parsed.scheme() == "ssh" && *port == 22));
    let authority = port.map_or(host.clone(), |port| format!("{host}:{port}"));
    let key = Sha256::digest(format!("{authority}/{repository}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(devcoordinator2_api::results::TestRepositorySource { key, name })
}

fn run_git_arguments(
    probe: &Path,
    run_as: Option<(u32, u32)>,
    arguments: &[&str],
    timeout: Duration,
) -> io::Result<GitOutput> {
    let git = trusted_executable("git")?;
    let drop_identity = run_as.filter(|(uid, _)| effective_uid() == 0 && *uid != 0);
    let mut command = if let Some((uid, gid)) = drop_identity {
        let home = passwd_home(uid)?;
        let setpriv = trusted_executable("setpriv")?;
        let mut command = Command::new(setpriv);
        command
            .arg(format!("--reuid={uid}"))
            .arg(format!("--regid={gid}"))
            .arg("--init-groups")
            .arg("--")
            .arg(&git)
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env("HOME", home);
        command
    } else {
        let mut command = Command::new(git);
        command
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env(
                "HOME",
                std::env::var_os("HOME").unwrap_or_else(|| OsString::from("/root")),
            )
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "safe.directory")
            .env("GIT_CONFIG_VALUE_0", "*");
        command
    };
    command
        .arg("-C")
        .arg(probe)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command_with_timeout(command, timeout)
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command_with_timeout(mut command: Command, timeout: Duration) -> io::Result<GitOutput> {
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("git stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("git stderr was not captured"))?;
    let stdout_reader = thread::Builder::new()
        .name("devcoordinator2-git-stdout".to_owned())
        .spawn(move || read_bounded(stdout, PROCESS_OUTPUT_CAP))?;
    let stderr_reader = thread::Builder::new()
        .name("devcoordinator2-git-stderr".to_owned())
        .spawn(move || read_bounded(stderr, PROCESS_OUTPUT_CAP))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {} seconds", timeout.as_secs()),
            ));
        }
        thread::sleep(PROCESS_POLL.min(deadline.saturating_duration_since(now)));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("git stdout reader failed"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("git stderr reader failed"))??;
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded(mut input: impl Read, cap: usize) -> io::Result<Vec<u8>> {
    let mut kept = Vec::with_capacity(cap.min(4096));
    let mut buffer = [0_u8; 4096];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(kept)
}

fn parse_git_result(output: GitOutput) -> Result<WorktreeInfo, ProtocolError> {
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        let error = truncate_text(error.trim(), GIT_ERROR_CAP);
        return Err(repository_not_found(if error.is_empty() {
            "not a git repository".to_owned()
        } else {
            error
        }));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    if lines.len() != 2 || lines[0].is_empty() {
        return Err(repository_not_found(
            "bare or unusable repository (no worktree)",
        ));
    }
    let worktree_root = Path::new(lines[0])
        .canonicalize()
        .map_err(|error| repository_not_found(format!("cannot resolve worktree: {error}")))?;
    let common_dir = Path::new(lines[1])
        .canonicalize()
        .map_err(|error| repository_not_found(format!("cannot resolve git common dir: {error}")))?;
    if common_dir.file_name() != Some(OsStr::new(".git")) {
        return Err(repository_not_found(format!(
            "unsupported repository layout: {}",
            common_dir.display()
        )));
    }
    let repository_root = common_dir.parent().ok_or_else(|| {
        repository_not_found(format!(
            "unsupported repository layout: {}",
            common_dir.display()
        ))
    })?;
    Ok(WorktreeInfo {
        repository_root: repository_root.to_owned(),
        worktree_root,
    })
}

fn trusted_executable(name: &str) -> io::Result<PathBuf> {
    for directory in ["/usr/bin", "/bin"] {
        let candidate = Path::new(directory).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name} was not found in {EXEC_PATH}"),
    ))
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
    unsafe { libc::geteuid() }
}

fn passwd_home(uid: u32) -> io::Result<OsString> {
    let suggested =
        // SAFETY: `sysconf` has no pointer arguments and `_SC_GETPW_R_SIZE_MAX` is valid.
        unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    };
    let mut storage = vec![0_u8; size.max(1024)];
    // SAFETY: A zeroed `passwd` is a valid output buffer for `getpwuid_r`.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: All pointers refer to live writable storage for the stated lengths.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut result,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if result.is_null() || entry.pw_dir.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown caller uid {uid}"),
        ));
    }
    // SAFETY: A successful `getpwuid_r` supplies a NUL-terminated `pw_dir`
    // pointer backed by `storage`, which remains alive through this copy.
    let bytes = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes().to_vec();
    Ok(OsString::from_vec(bytes))
}

fn persistent_archive_blocker(
    connection: &Connection,
    repository_id: &str,
) -> Result<Option<String>, DatabaseError> {
    for (label, sql) in [
        (
            "open planning work",
            "SELECT 1 FROM tasks WHERE repository_id=?1 \
             AND (status IN ('planned','in_progress') OR elaboration_needed=1) LIMIT 1",
        ),
        (
            "open release work",
            "SELECT 1 FROM releases WHERE repository_id=?1 \
             AND status IN ('planned','requested') LIMIT 1",
        ),
        (
            "active deployments",
            "SELECT 1 FROM deployments WHERE repository_id=?1 \
             AND state NOT IN ('stopped','failed') LIMIT 1",
        ),
        (
            "active observed deployments",
            "SELECT 1 FROM observed_deployments WHERE repository_id=?1 \
             AND state='running' LIMIT 1",
        ),
    ] {
        if connection
            .query_row(sql, [repository_id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(Some(format!("repository still has {label}")));
        }
    }
    Ok(None)
}

fn select_repository(
    connection: &Connection,
    repository_id: &str,
) -> Result<Option<Repository>, DatabaseError> {
    Ok(connection
        .query_row(
            "SELECT repository_id,root_path,display_name,registered_at,registered_by_uid,\
                    last_seen_at,archived_at,archived_by_uid,archive_note,\
                    merged_into_repository_id \
             FROM repositories WHERE repository_id=?1",
            [repository_id],
            repository_row,
        )
        .optional()?)
}

fn repository_row(row: &Row<'_>) -> rusqlite::Result<Repository> {
    Ok(Repository {
        repository_id: row.get(0)?,
        root_path: row.get(1)?,
        display_name: row.get(2)?,
        registered_at: row.get(3)?,
        registered_by_uid: integer_u32(row, 4)?,
        last_seen_at: row.get(5)?,
        archived_at: row.get(6)?,
        archived_by_uid: optional_integer_u32(row, 7)?,
        archive_note: row.get(8)?,
        merged_into_repository_id: row.get(9)?,
    })
}

fn repository_list_row(row: &Row<'_>) -> rusqlite::Result<RepositoryListRow> {
    Ok(RepositoryListRow {
        repository_id: row.get(0)?,
        root_path: row.get(1)?,
        display_name: row.get(2)?,
        registered_at: row.get(3)?,
        last_seen_at: row.get(4)?,
        archived_at: row.get(5)?,
        archived_by_uid: optional_integer_u32(row, 6)?,
        archive_note: row.get(7)?,
        merged_into_repository_id: row.get(8)?,
        worktrees: Vec::new(),
    })
}

fn integer_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value = row.get::<_, i64>(index)?;
    u32::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn optional_integer_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u32>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| {
            u32::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn validate_absolute_path(path: &Path) -> Result<(), ProtocolError> {
    if path.as_os_str().is_empty() {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "'path' (string) is required",
        ));
    }
    if !path.is_absolute() {
        return Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "'path' must be absolute",
        ));
    }
    Ok(())
}

fn validate_repository_id(value: &str, field: &str) -> Result<(), ProtocolError> {
    let valid = value.len() == 17
        && value.starts_with('r')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            format!("'{field}' must be an 'r…' id"),
        ))
    }
}

fn validate_note(note: &str) -> Result<(), ProtocolError> {
    if (3..=500).contains(&note.chars().count()) && !note.contains(['\n', '\r']) {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::ParamsInvalid,
            "'note' must be one 3..500 character line",
        ))
    }
}

fn timestamp() -> Result<String, ProtocolError> {
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .map_err(|error| {
            ProtocolError::new(
                ErrorCode::InternalError,
                "could not format repository timestamp",
            )
            .with_detail(error.to_string())
        })
}

fn path_text(path: &Path) -> Result<String, ProtocolError> {
    path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::RepositoryNotFound,
            "repository path is not valid UTF-8",
        )
    })
}

fn repository_not_found(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::RepositoryNotFound, message)
}

fn archive_blocked(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::RepositoryArchiveBlocked, message)
}

fn database_error(error: DatabaseError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "repository database operation failed",
    )
    .with_detail(error.to_string())
}

fn id_error(error: ids::IdError) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InternalError,
        "could not derive repository identity",
    )
    .with_detail(error.to_string())
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use tempfile::{TempDir, tempdir};

    #[test]
    fn test_repository_source_groups_origins_without_exposing_credentials() {
        let expected =
            repository_source_from_remote("https://github.com/owner/project.git").unwrap();
        assert_eq!(expected.name, "project");
        for remote in [
            "git@github.com:owner/project.git",
            "ssh://git@github.com:22/owner/project",
            "https://user:private-token@github.com/owner/project.git",
        ] {
            assert_eq!(repository_source_from_remote(remote).unwrap(), expected);
        }
        for remote in [
            "https://git.example/owner/project.git",
            "https://github.com/another/project.git",
            "https://github.com/owner/Project.git",
        ] {
            assert_ne!(
                repository_source_from_remote(remote).unwrap().key,
                expected.key
            );
        }
        for remote in [
            "",
            "/private/project",
            "file:///private/project",
            "https://github.com/owner/project?token=private",
            "https://github.com/owner/project#private",
            "https://github.com/owner/%70roject",
        ] {
            assert!(repository_source_from_remote(remote).is_none());
        }
    }

    #[test]
    fn test_repository_source_reads_the_declared_origin_of_a_checkout() {
        let fixture = RepositoryFixture::new();
        assert!(test_repository_source(&fixture.root, identity()).is_none());
        git(
            &fixture.root,
            &[
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                OsStr::new("https://github.com/owner/actual-repository.git"),
            ],
        );
        let source = test_repository_source(&fixture.root, identity()).unwrap();
        assert_eq!(source.name, "actual-repository");
        assert_ne!(
            source.name,
            fixture.root.file_name().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn test_repository_source_follows_local_clones_and_linked_worktrees() {
        let fixture = RepositoryFixture::new();
        git(
            &fixture.root,
            &[
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                OsStr::new("https://github.com/owner/hdlripper.git"),
            ],
        );
        let linked = fixture
            .root
            .parent()
            .unwrap()
            .join("hdlripper-windows-0.1.4");
        git(
            &fixture.root,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                linked.as_os_str(),
            ],
        );
        let expected = test_repository_source(&fixture.root, identity());
        assert!(expected.is_some());
        for root in [&fixture.root, &linked] {
            let workspace = root.join(".local/daily/workspace");
            std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
            git(
                root,
                &[
                    OsStr::new("clone"),
                    OsStr::new("--quiet"),
                    root.as_os_str(),
                    workspace.as_os_str(),
                ],
            );
            assert_eq!(test_repository_source(&workspace, identity()), expected);
            git(
                &workspace,
                &[
                    OsStr::new("remote"),
                    OsStr::new("set-url"),
                    OsStr::new("origin"),
                    OsStr::new("../../.."),
                ],
            );
            assert_eq!(test_repository_source(&workspace, identity()), expected);
            let file_url = reqwest::Url::from_directory_path(root).unwrap().to_string();
            git(
                &workspace,
                &[
                    OsStr::new("remote"),
                    OsStr::new("set-url"),
                    OsStr::new("origin"),
                    OsStr::new(&file_url),
                ],
            );
            assert_eq!(test_repository_source(&workspace, identity()), expected);
        }
    }

    #[test]
    fn test_repository_source_rejects_cycles_missing_paths_and_parent_inference() {
        let fixture = RepositoryFixture::new();
        let nested = fixture.root.join("workspace");
        create_repository(&nested);
        git(
            &fixture.root,
            &[
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                OsStr::new("https://github.com/owner/hdlripper.git"),
            ],
        );
        assert!(test_repository_source(&nested, identity()).is_none());
        for remote in [
            ".",
            "../missing",
            "file://other-host/private/repo",
            "https://github.com/owner/repo?token=private",
        ] {
            git(
                &nested,
                &[
                    OsStr::new("config"),
                    OsStr::new("remote.origin.url"),
                    OsStr::new(remote),
                ],
            );
            assert!(test_repository_source(&nested, identity()).is_none());
        }
        let ordinary = fixture.root.join("ordinary-subdirectory");
        std::fs::create_dir(&ordinary).unwrap();
        git(
            &nested,
            &[
                OsStr::new("config"),
                OsStr::new("remote.origin.url"),
                ordinary.as_os_str(),
            ],
        );
        assert!(test_repository_source(&nested, identity()).is_none());
        git(
            &nested,
            &[
                OsStr::new("config"),
                OsStr::new("remote.origin.url"),
                fixture.root.as_os_str(),
            ],
        );
        git(
            &fixture.root,
            &[
                OsStr::new("remote"),
                OsStr::new("set-url"),
                OsStr::new("origin"),
                nested.as_os_str(),
            ],
        );
        assert!(test_repository_source(&nested, identity()).is_none());
    }

    struct RepositoryFixture {
        _temporary: TempDir,
        root: PathBuf,
        database: Database,
        registry: Registry,
    }

    impl RepositoryFixture {
        fn new() -> Self {
            let temporary = tempdir().expect("temporary directory");
            let root = temporary.path().join("source");
            create_repository(&root);
            let database =
                Database::open(temporary.path().join("authority.sqlite3")).expect("database");
            let registry = Registry::new(database.clone());
            Self {
                _temporary: temporary,
                root,
                database,
                registry,
            }
        }
    }

    fn identity() -> (u32, u32) {
        // SAFETY: These libc identity getters have no preconditions.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn git(cwd: &Path, arguments: &[&OsStr]) {
        let git = trusted_executable("git").expect("git");
        let status = Command::new(git)
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", EXEC_PATH)
            .env("HOME", cwd)
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed");
    }

    fn create_repository(root: &Path) {
        std::fs::create_dir_all(root).expect("repository directory");
        git(root, &[OsStr::new("init"), OsStr::new("-q")]);
        std::fs::write(root.join("tracked.txt"), b"fixture").expect("fixture file");
        git(root, &[OsStr::new("add"), OsStr::new("tracked.txt")]);
        git(
            root,
            &[
                OsStr::new("commit"),
                OsStr::new("-q"),
                OsStr::new("-m"),
                OsStr::new("initial"),
            ],
        );
    }

    #[test]
    fn linked_worktrees_share_identity_and_repeat_registration_is_idempotent() {
        let fixture = RepositoryFixture::new();
        let (uid, gid) = identity();
        let tracked_file = fixture.root.join("tracked.txt");
        let first = fixture
            .registry
            .register(&tracked_file, uid, gid)
            .expect("register source");
        assert!(first.registered);
        let repeated = fixture
            .registry
            .register(&fixture.root, uid, gid)
            .expect("register again");
        assert!(!repeated.registered);
        assert_eq!(first.repository_id, repeated.repository_id);
        assert_eq!(first.worktree_id, repeated.worktree_id);

        let linked_root = fixture._temporary.path().join("linked");
        git(
            &fixture.root,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-q"),
                linked_root.as_os_str(),
            ],
        );
        let linked = fixture
            .registry
            .register(&linked_root, uid, gid)
            .expect("register linked worktree");
        assert_eq!(first.repository_id, linked.repository_id);
        assert_ne!(first.worktree_id, linked.worktree_id);

        let listed = fixture
            .registry
            .list_repositories(false)
            .expect("list repositories");
        assert_eq!(listed.repositories.len(), 1);
        assert_eq!(listed.repositories[0].worktrees.len(), 2);
        let status = fixture
            .registry
            .repository_status(&linked_root, Some((uid, gid)))
            .expect("repository status");
        assert_eq!(status.worktree_id, linked.worktree_id);
        assert_eq!(status.current_test, None);
    }

    #[test]
    fn archive_and_unarchive_preserve_permanent_history() {
        let fixture = RepositoryFixture::new();
        let target_root = fixture._temporary.path().join("target");
        create_repository(&target_root);
        let (uid, gid) = identity();
        let source = fixture
            .registry
            .register(&fixture.root, uid, gid)
            .expect("register source");
        let target = fixture
            .registry
            .register(&target_root, uid, gid)
            .expect("register target");

        let archived = fixture
            .registry
            .archive(
                &source.repository_id,
                &target.repository_id,
                "Merged skills",
                uid,
            )
            .expect("archive");
        assert!(archived.archived_at.is_some());
        assert_eq!(
            archived.merged_into_repository_id.as_deref(),
            Some(target.repository_id.as_str())
        );
        assert_eq!(
            fixture
                .registry
                .list_repositories(false)
                .expect("active repositories")
                .repositories
                .iter()
                .map(|row| row.repository_id.as_str())
                .collect::<Vec<_>>(),
            vec![target.repository_id.as_str()]
        );
        let archived_error = fixture
            .registry
            .register(&fixture.root, uid, gid)
            .expect_err("archived source is refused");
        assert_eq!(archived_error.code, ErrorCode::RepositoryArchived);

        let restored = fixture
            .registry
            .unarchive(&source.repository_id, "Rollback consolidation", uid)
            .expect("unarchive");
        assert!(restored.archived_at.is_none());
        let events: Vec<(String, String, u32)> = fixture
            .database
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT event,note,actor_uid FROM repository_events \
                     WHERE repository_id=?1 ORDER BY event_id",
                )?;
                Ok(statement
                    .query_map([source.repository_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, integer_u32(row, 2)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .expect("repository history");
        assert_eq!(
            events,
            vec![
                ("archived".to_owned(), "Merged skills".to_owned(), uid),
                (
                    "unarchived".to_owned(),
                    "Rollback consolidation".to_owned(),
                    uid,
                ),
            ]
        );
    }

    #[test]
    fn archive_refuses_open_planning_work() {
        let fixture = RepositoryFixture::new();
        let target_root = fixture._temporary.path().join("target");
        create_repository(&target_root);
        let (uid, gid) = identity();
        let source = fixture
            .registry
            .register(&fixture.root, uid, gid)
            .expect("register source");
        let target = fixture
            .registry
            .register(&target_root, uid, gid)
            .expect("register target");
        let repository_id = source.repository_id.clone();
        fixture
            .database
            .transaction(move |connection| {
                connection.execute(
                    "INSERT INTO tasks(\
                       task_id,repository_id,seq,position,title,outcome,kind,status,\
                       created_at,created_by,updated_at\
                     ) VALUES('p1',?1,1,1,'Open work','Open work','improvement',\
                              'planned','t','fixture','t')",
                    [repository_id],
                )?;
                Ok(())
            })
            .expect("open task");

        let error = fixture
            .registry
            .archive(
                &source.repository_id,
                &target.repository_id,
                "Too early",
                uid,
            )
            .expect_err("open task blocks archive");
        assert_eq!(error.code, ErrorCode::RepositoryArchiveBlocked);
        assert_eq!(error.message, "repository still has open planning work");
    }

    #[test]
    fn invalid_repository_and_path_have_stable_protocol_errors() {
        let fixture = RepositoryFixture::new();
        let (uid, gid) = identity();
        let relative = fixture
            .registry
            .register(Path::new("relative"), uid, gid)
            .expect_err("relative path");
        assert_eq!(relative.code, ErrorCode::ParamsInvalid);

        let non_repository = fixture._temporary.path().join("not-a-repository");
        std::fs::create_dir(&non_repository).expect("non-repository");
        // Stop Git discovery at the fixture even when the test temporary
        // directory itself lives beneath a real checkout.
        std::fs::write(
            non_repository.join(".git"),
            "gitdir: /devcoordinator2-test/nonexistent\n",
        )
        .expect("invalid git boundary");
        let missing = fixture
            .registry
            .register(&non_repository, uid, gid)
            .expect_err("non-repository");
        assert_eq!(missing.code, ErrorCode::RepositoryNotFound);
        assert!(missing.message.len() <= GIT_ERROR_CAP);
    }

    #[test]
    fn git_failure_text_is_bounded_on_utf8_boundaries() {
        let error = parse_git_result(GitOutput {
            status: ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: "é".repeat(600).into_bytes(),
        })
        .expect_err("failed git");
        assert_eq!(error.code, ErrorCode::RepositoryNotFound);
        assert!(error.message.len() <= GIT_ERROR_CAP);
        assert!(std::str::from_utf8(error.message.as_bytes()).is_ok());
    }
}
